use anyhow::{Context, Result, bail};
use clap::Parser;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

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
            "-r",
            "60",
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

fn main() -> Result<()> {
    let args = Args::parse();

    let inputs = collect_inputs(&args.input_dir)?;
    if inputs.is_empty() {
        bail!("no .mp4 files found in {}", args.input_dir.display());
    }

    let filter = build_video_filter(&args.lut)?;
    let output = output_path(&args.output_dir, &earliest_date(&inputs)?);
    let concat_list = write_concat_list(&inputs)?;
    run_ffmpeg(concat_list.path(), &filter, &output)
}
