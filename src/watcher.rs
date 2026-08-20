use crate::pipeline;
use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, watch};

pub enum WaitOutcome {
    Batch(Vec<PathBuf>),
    Shutdown,
}

pub struct DirWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
}

impl DirWatcher {
    pub fn new(dir: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("failed to create filesystem watcher")?;
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {}", dir.display()))?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Drops any events currently queued without acting on them. Call this
    /// right after moving a batch's clips out of `input_dir` — those moves
    /// themselves generate events (the vacated source paths), which would
    /// otherwise spuriously seed the next call's quiet-period timer.
    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// Blocks until `input_dir` has been quiet (no filesystem events) for
    /// `quiet_period`, then returns the files currently in it as one batch.
    /// Returns `Shutdown` if the shutdown signal fires first.
    pub async fn wait_for_batch(
        &mut self,
        input_dir: &Path,
        quiet_period: Duration,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<WaitOutcome> {
        loop {
            let existing = pipeline::collect_inputs(input_dir)?;

            let mut last_activity = if existing.is_empty() {
                // Nothing to batch yet: block on the next event (or shutdown)
                // rather than running a timer against an empty directory.
                tokio::select! {
                    _ = shutdown.changed() => return Ok(WaitOutcome::Shutdown),
                    ev = self.rx.recv() => {
                        ev.context("filesystem watcher channel closed")?
                            .context("filesystem watcher error")?;
                        SystemTime::now()
                    }
                }
            } else {
                // Files already present (e.g. service just (re)started mid-session):
                // seed from their newest mtime instead of `now()` so we don't force
                // an extra full quiet_period wait for clips that already settled.
                newest_mtime(&existing)?
            };

            loop {
                let elapsed = SystemTime::now()
                    .duration_since(last_activity)
                    .unwrap_or_default();
                if elapsed >= quiet_period {
                    break;
                }
                tokio::select! {
                    _ = shutdown.changed() => return Ok(WaitOutcome::Shutdown),
                    ev = self.rx.recv() => {
                        ev.context("filesystem watcher channel closed")?
                            .context("filesystem watcher error")?;
                        last_activity = SystemTime::now();
                    }
                    _ = tokio::time::sleep(quiet_period - elapsed) => break,
                }
            }

            let batch = pipeline::collect_inputs(input_dir)?;
            if !batch.is_empty() {
                return Ok(WaitOutcome::Batch(batch));
            }
            // Everything vanished mid-wait (e.g. manual deletion) — loop back to idle.
        }
    }
}

fn newest_mtime(paths: &[PathBuf]) -> Result<SystemTime> {
    paths
        .iter()
        .map(|path| {
            path.metadata()
                .and_then(|m| m.modified())
                .with_context(|| format!("failed to read mtime of {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .context("no input files")
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        sig.recv().await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
