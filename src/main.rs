mod gpu;
mod pipeline;
mod watcher;

use anyhow::{Context, Result};
use clap::Parser;
use log::{error, info};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use watcher::WaitOutcome;

/// Watches a directory for DJI footage, and for each complete session
/// (concatenate clips chronologically, crop to 16:9, apply a LUT, encode to
/// HEVC), uploads the result to YouTube.
#[derive(Parser)]
struct Args {
    /// Directory to watch for input .mp4 clips
    #[arg(long, default_value = "input")]
    input_dir: PathBuf,

    /// Directory to write output files into
    #[arg(long, default_value = "output")]
    output_dir: PathBuf,

    /// Directory successfully-processed clips are archived into
    #[arg(long, default_value = "processed")]
    processed_dir: PathBuf,

    /// Directory clips from a failed batch are moved into (as failed_dir/<timestamp>/)
    #[arg(long, default_value = "failed")]
    failed_dir: PathBuf,

    /// LUT file to apply
    #[arg(
        long,
        default_value = "luts/DJI OSMO Action 6 D-LogM to Rec.709 LUT-11.17.cube"
    )]
    lut: PathBuf,

    /// Seconds of filesystem quiet in input_dir before a batch is considered complete
    #[arg(long, default_value_t = 300)]
    quiet_period_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("failed to create {}", args.output_dir.display()))?;
    fs::create_dir_all(&args.processed_dir)
        .with_context(|| format!("failed to create {}", args.processed_dir.display()))?;
    fs::create_dir_all(&args.failed_dir)
        .with_context(|| format!("failed to create {}", args.failed_dir.display()))?;

    let gpu_arch = gpu::detect_gpu_arch()?;
    info!(
        "detected GPU arch {gpu_arch:?} (temporal_aq {})",
        if gpu_arch == gpu::GpuArch::TuringOrNewer {
            "enabled"
        } else {
            "disabled"
        }
    );

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        watcher::shutdown_signal().await;
        info!("shutdown requested; will exit after the current batch (if any) finishes");
        let _ = shutdown_tx.send(true);
    });

    let mut watch = watcher::DirWatcher::new(&args.input_dir)?;
    let quiet_period = Duration::from_secs(args.quiet_period_secs);

    info!("watching {} for new footage", args.input_dir.display());
    loop {
        let batch = match watch
            .wait_for_batch(&args.input_dir, quiet_period, &mut shutdown_rx)
            .await?
        {
            WaitOutcome::Shutdown => break,
            WaitOutcome::Batch(inputs) => inputs,
        };
        info!("batch ready: {} clip(s)", batch.len());

        match pipeline::process_batch(&batch, &args.output_dir, &args.lut, gpu_arch).await {
            Ok(pipeline::BatchOutcome::Uploaded { date }) => {
                info!(
                    "batch succeeded (date {date}), archiving clips to {}",
                    args.processed_dir.display()
                );
                if let Err(err) = move_batch(&batch, &args.processed_dir) {
                    error!(
                        "batch processed but failed to archive clips to {}: {err:#}",
                        args.processed_dir.display()
                    );
                }
            }
            Ok(pipeline::BatchOutcome::UploadFailed { date }) => {
                error!(
                    "upload failed for batch (date {date}); video saved locally, \
                     archiving clips to {} anyway (re-encoding won't help — retry the upload manually)",
                    args.processed_dir.display()
                );
                if let Err(err) = move_batch(&batch, &args.processed_dir) {
                    error!(
                        "additionally failed to archive clips to {}: {err:#}",
                        args.processed_dir.display()
                    );
                }
            }
            Err(err) => {
                error!("batch failed: {err:#}");
                let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
                let dest = args.failed_dir.join(ts.to_string());
                if let Err(err) = move_batch(&batch, &dest) {
                    error!(
                        "additionally failed to move clips to {}: {err:#}",
                        dest.display()
                    );
                }
            }
        }

        watch.drain_pending();
    }

    info!("shutdown complete");
    Ok(())
}

fn move_batch(inputs: &[PathBuf], dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;
    for input in inputs {
        let name = input
            .file_name()
            .with_context(|| format!("input path has no file name: {}", input.display()))?;
        fs::rename(input, dest_dir.join(name)).with_context(|| {
            format!(
                "failed to move {} to {}",
                input.display(),
                dest_dir.display()
            )
        })?;
    }
    Ok(())
}
