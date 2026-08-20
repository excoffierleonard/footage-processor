use crate::gpu::GpuArch;
use anyhow::{Context, Result, bail};
use google_youtube3::api::{
    PlaylistItem, PlaylistItemSnippet, ResourceId, Video, VideoSnippet, VideoStatus,
};
use google_youtube3::{YouTube, hyper_rustls, hyper_util, yup_oauth2};
use log::{info, log_enabled, warn};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

const CLIENT_SECRET_PATH: &str = "client_secret.json";
const TOKEN_CACHE_PATH: &str = "youtube_token.json";
const PLAYLIST_ID: &str = "PLrDx50RI8LwOsr-hccwOgM_BbTUxux5lf";

pub fn collect_inputs(dir: &Path) -> Result<Vec<PathBuf>> {
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
pub fn earliest_date(inputs: &[PathBuf]) -> Result<String> {
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

pub fn output_path(dir: &Path, date: &str) -> PathBuf {
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

pub fn build_video_filter(lut: &Path) -> Result<String> {
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

fn run_ffmpeg(concat_list: &Path, filter: &str, output: &Path, gpu_arch: GpuArch) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-f", "concat", "-safe", "0", "-i"])
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
        ]);

    if gpu_arch == GpuArch::TuringOrNewer {
        cmd.args(["-temporal_aq", "1"]);
    }

    cmd.args([
        "-multipass",
        "fullres",
        "-pix_fmt",
        "p010le",
        "-c:a",
        "copy",
    ])
    .arg(output);

    // ffmpeg's own progress/banner output is noisy; only stream it live when
    // debug logging is enabled. At the default (info) level it's captured
    // and only surfaced if the encode actually fails.
    if log_enabled!(log::Level::Debug) {
        let status = cmd.status().context("failed to run ffmpeg")?;
        if !status.success() {
            bail!("ffmpeg exited with {status}");
        }
    } else {
        let result = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("failed to run ffmpeg")?;
        if !result.status.success() {
            bail!(
                "ffmpeg exited with {}\n{}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    Ok(())
}

/// Uploads a video as a private, unlisted-by-default `YouTube` upload. Requires
/// a Google Cloud OAuth client (see CLAUDE.md / project README for setup);
/// the first run opens a browser for consent, after which the refresh token
/// is cached at `TOKEN_CACHE_PATH` and later runs are silent. Returns the
/// uploaded video's id.
async fn upload_video(video: &Path, title: &str) -> Result<String> {
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
    uploaded
        .id
        .context("YouTube didn't return an id for the uploaded video")
}

async fn add_to_playlist(video_id: &str, playlist_id: &str) -> Result<()> {
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

    let playlist_item = PlaylistItem {
        snippet: Some(PlaylistItemSnippet {
            playlist_id: Some(playlist_id.to_string()),
            resource_id: Some(ResourceId {
                kind: Some("youtube#video".to_string()),
                video_id: Some(video_id.to_string()),
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

/// Outcome of a successfully *encoded* batch — the source clips are always
/// safe to archive once `process_batch` returns `Ok`, regardless of variant.
pub enum BatchOutcome {
    /// Uploaded (and, best-effort, playlisted).
    Uploaded { date: String },
    /// Encode succeeded and the video is sitting in `output_dir`, but the
    /// YouTube upload itself failed (e.g. a network blip). Not treated as a
    /// batch failure: re-running the batch would re-encode for nothing and
    /// still risk re-uploading a duplicate if the upload had actually landed.
    UploadFailed { date: String },
}

/// Runs one batch (a session's worth of clips) through the full pipeline:
/// concat + encode + upload + playlist.
///
/// Only an encode failure returns `Err` (routing the batch to `failed/` for
/// manual retry). A playlist-insert failure after a successful upload is
/// logged but doesn't fail the batch — retrying would re-upload a duplicate
/// video, which is worse than a video missing from the playlist.
pub async fn process_batch(
    inputs: &[PathBuf],
    output_dir: &Path,
    lut: &Path,
    gpu_arch: GpuArch,
) -> Result<BatchOutcome> {
    let filter = build_video_filter(lut)?;
    let date = earliest_date(inputs)?;
    let output = output_path(output_dir, &date);
    let concat_list = write_concat_list(inputs)?;

    info!("encoding to {}", output.display());
    run_ffmpeg(concat_list.path(), &filter, &output, gpu_arch)?;
    info!("encode complete");

    info!("uploading to YouTube");
    let video_id = match upload_video(&output, &format!("Motovlog - {date}")).await {
        Ok(id) => id,
        Err(err) => {
            warn!("upload failed for {}: {err:#}", output.display());
            return Ok(BatchOutcome::UploadFailed { date });
        }
    };
    info!("uploaded video {video_id}, adding to playlist");

    if let Err(err) = add_to_playlist(&video_id, PLAYLIST_ID).await {
        warn!("video {video_id} uploaded but not added to playlist; add manually: {err:#}");
    }

    Ok(BatchOutcome::Uploaded { date })
}
