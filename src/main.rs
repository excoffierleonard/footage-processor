mod gpu;
mod pipeline;
mod watcher;

use anyhow::{Context, Result};
use google_youtube3::yup_oauth2;
use log::{error, info};
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::Builder;
use watcher::WaitOutcome;

const INPUT_DIR: &str = "input";
const OUTPUT_DIR: &str = "output";
const PROCESSED_DIR: &str = "processed";
const FAILED_DIR: &str = "failed";
const QUIET_PERIOD_SECS: u64 = 300;

/// Embedded at build time so the container image needs no LUT file or mount.
const LUT_BYTES: &[u8] =
    include_bytes!("../luts/DJI OSMO Action 6 D-LogM to Rec.709 LUT-11.17.cube");

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let input_dir = Path::new(INPUT_DIR);
    let output_dir = Path::new(OUTPUT_DIR);
    let processed_dir = Path::new(PROCESSED_DIR);
    let failed_dir = Path::new(FAILED_DIR);

    let mut lut_file = Builder::new()
        .suffix(".cube")
        .tempfile()
        .context("failed to create temp file for embedded LUT")?;
    lut_file
        .write_all(LUT_BYTES)
        .context("failed to write embedded LUT to temp file")?;
    let lut = lut_file.path();

    // One env var carries both pieces yup_oauth2 needs: `client_secret` (a
    // Google OAuth "Desktop app" client — see docs/oauth-setup.md) and its
    // cached refresh `token`. Shape:
    // {"client_secret": <client_secret.json content>, "token": <youtube_token.json content>}
    let raw_credentials =
        env::var("YOUTUBE_CREDENTIALS").context("YOUTUBE_CREDENTIALS env var not set")?;
    let credentials: serde_json::Value = serde_json::from_str(&raw_credentials)
        .context("failed to parse YOUTUBE_CREDENTIALS as JSON")?;

    let client_secret_json = credentials
        .get("client_secret")
        .context("YOUTUBE_CREDENTIALS missing \"client_secret\"")?;
    let client_secret = yup_oauth2::parse_application_secret(client_secret_json.to_string())
        .context("failed to parse \"client_secret\" from YOUTUBE_CREDENTIALS")?;

    // yup_oauth2 treats a missing cache file as "no token yet" and starts the
    // interactive consent flow; an existing-but-empty file would instead fail
    // to parse. So when YOUTUBE_CREDENTIALS carries no token, leave this path
    // unwritten.
    let token_cache_dir =
        tempfile::tempdir().context("failed to create temp dir for YouTube token cache")?;
    let token_cache = token_cache_dir.path().join("youtube_token.json");
    if let Some(token_json) = credentials.get("token") {
        fs::write(&token_cache, token_json.to_string())
            .context("failed to write token from YOUTUBE_CREDENTIALS to temp file")?;
    }

    fs::create_dir_all(output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    fs::create_dir_all(processed_dir)
        .with_context(|| format!("failed to create {}", processed_dir.display()))?;
    fs::create_dir_all(failed_dir)
        .with_context(|| format!("failed to create {}", failed_dir.display()))?;

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

    let mut watch = watcher::DirWatcher::new(input_dir)?;
    let quiet_period = Duration::from_secs(QUIET_PERIOD_SECS);

    info!("watching {} for new footage", input_dir.display());
    loop {
        let batch = match watch
            .wait_for_batch(input_dir, quiet_period, &mut shutdown_rx)
            .await?
        {
            WaitOutcome::Shutdown => break,
            WaitOutcome::Batch(inputs) => inputs,
        };
        info!("batch ready: {} clip(s)", batch.len());

        match pipeline::process_batch(
            &batch,
            output_dir,
            lut,
            &client_secret,
            &token_cache,
            gpu_arch,
        )
        .await
        {
            Ok(pipeline::BatchOutcome::Uploaded { date }) => {
                info!(
                    "batch succeeded (date {date}), archiving clips to {}",
                    processed_dir.display()
                );
                if let Err(err) = move_batch(&batch, processed_dir) {
                    error!(
                        "batch processed but failed to archive clips to {}: {err:#}",
                        processed_dir.display()
                    );
                }
            }
            Ok(pipeline::BatchOutcome::UploadFailed { date }) => {
                error!(
                    "upload failed for batch (date {date}); video saved locally, \
                     archiving clips to {} anyway (re-encoding won't help — retry the upload manually)",
                    processed_dir.display()
                );
                if let Err(err) = move_batch(&batch, processed_dir) {
                    error!(
                        "additionally failed to archive clips to {}: {err:#}",
                        processed_dir.display()
                    );
                }
            }
            Err(err) => {
                error!("batch failed: {err:#}");
                let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
                let dest = failed_dir.join(ts.to_string());
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
