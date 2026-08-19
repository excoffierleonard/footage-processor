use anyhow::{Context, Result, bail};
use clap::Parser;
use google_youtube3::api::{
    PlaylistItem, PlaylistItemSnippet, ResourceId, Video, VideoSnippet, VideoStatus,
};
use google_youtube3::{YouTube, hyper_rustls, hyper_util, yup_oauth2};
use log::info;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

const CLIENT_SECRET_PATH: &str = "client_secret.json";
const TOKEN_CACHE_PATH: &str = "youtube_token.json";
const PLAYLIST_ID: &str = "PLrDx50RI8LwOsr-hccwOgM_BbTUxux5lf";

/// Concatenate DJI clips chronologically, crop to 16:9, apply a LUT, and encode to HEVC.
#[derive(Parser)]
struct Args {
    /// Directory containing input .mp4 clips
    #[arg(long, default_value = "input")]
    input_dir: PathBuf,

    /// Directory to write the output file into
    #[arg(long, default_value = "output")]
    output_dir: PathBuf,

    /// LUT file to apply
    #[arg(
        long,
        default_value = "luts/DJI OSMO Action 6 D-LogM to Rec.709 LUT-11.17.cube"
    )]
    lut: PathBuf,
}

fn collect_inputs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut inputs: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("failed to read input directory {}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, std::io::Error>>()
        .with_context(|| format!("failed to list entries in {}", dir.display()))?
        .into_iter()
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
        })
        .collect();
    inputs.sort();
    Ok(inputs)
}

/// Earliest capture date (YYYYMMDD) across all inputs, parsed from DJI's
/// `DJI_YYYYMMDDHHMMSS_....mp4` naming convention. Errors rather than silently
/// skipping a file that doesn't match, since that file is still concatenated
/// into the output.
fn earliest_date(inputs: &[PathBuf]) -> Result<String> {
    inputs
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .with_context(|| format!("non-UTF8 filename: {}", path.display()))?;
            name.strip_prefix("DJI_")
                .and_then(|rest| rest.get(0..8))
                .with_context(|| format!("filename doesn't match DJI_YYYYMMDD...: {name}"))
        })
        .collect::<Result<Vec<&str>>>()?
        .into_iter()
        .min()
        .map(str::to_string)
        .context("no input files")
}

fn output_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("Motovlog - {date}.mp4"))
}

/// Escapes a value for use inside a single-quoted ffmpeg filtergraph literal.
fn escape_filter_literal(value: &str) -> String {
    value.replace('\'', r"'\''")
}

/// Escapes a value for use inside a single-quoted concat demuxer list entry.
fn escape_concat_literal(value: &str) -> String {
    value.replace('\\', r"\\").replace('\'', r"\'")
}

fn build_video_filter(lut: &Path) -> Result<String> {
    let lut = lut
        .to_str()
        .with_context(|| format!("LUT path is not valid UTF-8: {}", lut.display()))?;
    Ok(format!(
        "crop=iw:iw*9/16,lut3d=file='{}',setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
        escape_filter_literal(lut)
    ))
}

/// Writes a concat-demuxer filelist. The clips share identical codec
/// parameters, so ffmpeg can concatenate them at the packet level (no
/// decode) before handing the result to `-vf`/`-c:a copy`.
fn write_concat_list(inputs: &[PathBuf]) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new().context("failed to create concat list temp file")?;
    for input in inputs {
        let absolute = fs::canonicalize(input)
            .with_context(|| format!("failed to resolve path {}", input.display()))?;
        let absolute = absolute
            .to_str()
            .with_context(|| format!("path is not valid UTF-8: {}", absolute.display()))?;
        writeln!(file, "file '{}'", escape_concat_literal(absolute))
            .context("failed to write concat list")?;
    }
    Ok(file)
}

fn run_ffmpeg(concat_list: &Path, filter: &str, output: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(concat_list)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-vf"])
        .arg(filter)
        .args([
            "-c:v",
            "hevc_nvenc",
            "-preset",
            "p7",
            "-rc",
            "vbr",
            "-b:v",
            "68M", // Youtube tops out at 68 Mbps for 4K60 SDR, so we don't need to go higher than that
            "-rc-lookahead",
            "20",
            "-spatial_aq",
            "1",
            // no -temporal_aq: unsupported on Pascal (dev GTX 1070), only Turing+
            "-multipass",
            "fullres",
            "-pix_fmt",
            "p010le",
            "-c:a",
            "copy",
        ])
        .arg(output)
        .status()
        .context("failed to run ffmpeg")?;

    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    Ok(())
}

/// Uploads a video as a private, unlisted-by-default `YouTube` upload. Requires
/// a Google Cloud OAuth client (see CLAUDE.md / project README for setup);
/// the first run opens a browser for consent, after which the refresh token
/// is cached at `TOKEN_CACHE_PATH` and later runs are silent.
async fn upload_to_youtube(video: &Path, title: &str) -> Result<()> {
    let secret = yup_oauth2::read_application_secret(CLIENT_SECRET_PATH)
        .await
        .with_context(|| format!("failed to read {CLIENT_SECRET_PATH}"))?;

    let connector = || {
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("failed to load native TLS roots")
            .map(|b| b.https_or_http().enable_http2().build())
    };

    let auth_client =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector()?);
    let auth = yup_oauth2::InstalledFlowAuthenticator::with_client(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::HTTPRedirect,
        yup_oauth2::client::CustomHyperClientBuilder::from(auth_client),
    )
    .persist_tokens_to_disk(TOKEN_CACHE_PATH)
    .build()
    .await
    .context("failed to authenticate with Google")?;

    let hub_client =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector()?);
    let hub = YouTube::new(hub_client, auth);

    let video_resource = Video {
        snippet: Some(VideoSnippet {
            title: Some(title.to_string()),
            category_id: Some("2".to_string()), // Autos & Vehicles
            ..Default::default()
        }),
        status: Some(VideoStatus {
            privacy_status: Some("private".to_string()),
            contains_synthetic_media: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    let file =
        fs::File::open(video).with_context(|| format!("failed to open {}", video.display()))?;
    let (_, uploaded) = hub
        .videos()
        .insert(video_resource)
        .upload_resumable(file, "video/mp4".parse().expect("valid mime type"))
        .await
        .context("YouTube upload failed")?;
    let video_id = uploaded
        .id
        .context("YouTube didn't return an id for the uploaded video")?;
    info!("uploaded video {video_id}, adding to playlist");

    let playlist_item = PlaylistItem {
        snippet: Some(PlaylistItemSnippet {
            playlist_id: Some(PLAYLIST_ID.to_string()),
            resource_id: Some(ResourceId {
                kind: Some("youtube#video".to_string()),
                video_id: Some(video_id),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    hub.playlist_items()
        .insert(playlist_item)
        .doit()
        .await
        .context("failed to add video to playlist")?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    let inputs = collect_inputs(&args.input_dir)?;
    if inputs.is_empty() {
        bail!("no .mp4 files found in {}", args.input_dir.display());
    }
    info!("found {} input clip(s)", inputs.len());

    let filter = build_video_filter(&args.lut)?;
    let date = earliest_date(&inputs)?;
    let output = output_path(&args.output_dir, &date);
    let concat_list = write_concat_list(&inputs)?;

    info!("encoding to {}", output.display());
    run_ffmpeg(concat_list.path(), &filter, &output)?;
    info!("encode complete");

    info!("uploading to YouTube");
    Box::pin(upload_to_youtube(&output, &format!("Motovlog - {date}"))).await?;
    info!("upload complete");

    Ok(())
}
