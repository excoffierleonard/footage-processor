use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// NVENC feature set differs by GPU generation. `-temporal_aq` requires
/// Turing (compute capability 7.5) or newer; Pascal (dev GTX 1070, 6.1)
/// doesn't support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuArch {
    PascalOrOlder,
    TuringOrNewer,
}

const UNSUPPORTED_MARKER: &str = "Temporal AQ not supported";

/// Probes NVENC's `temporal_aq` support with a throwaway single-frame encode.
/// ffmpeg queries the hardware's NVENC capabilities itself and logs
/// `Temporal AQ not supported` when it does, regardless of whether the
/// encode goes on to succeed, so we key off that message rather than the
/// exit status.
pub fn detect_gpu_arch() -> Result<GpuArch> {
    let output = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "warning",
            "-f",
            "lavfi",
            "-i",
            "color=black:size=64x64:duration=0.1:rate=1",
            "-c:v",
            "hevc_nvenc",
            "-temporal_aq",
            "1",
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run ffmpeg (is it installed?)")?;

    Ok(arch_from_probe_stderr(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

fn arch_from_probe_stderr(stderr: &str) -> GpuArch {
    if stderr.contains(UNSUPPORTED_MARKER) {
        GpuArch::PascalOrOlder
    } else {
        GpuArch::TuringOrNewer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_logs_unsupported_warning() {
        assert_eq!(
            arch_from_probe_stderr("[hevc_nvenc @ 0x0] Temporal AQ not supported\n"),
            GpuArch::PascalOrOlder
        );
    }

    #[test]
    fn turing_and_ampere_accept_the_flag() {
        assert_eq!(arch_from_probe_stderr(""), GpuArch::TuringOrNewer);
        assert_eq!(
            arch_from_probe_stderr("[hevc_nvenc @ 0x0] Temporal AQ enabled.\n"),
            GpuArch::TuringOrNewer
        );
    }
}
