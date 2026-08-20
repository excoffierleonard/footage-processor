use anyhow::{Context, Result, bail};
use std::process::Command;

/// NVENC feature set differs by GPU generation. `-temporal_aq` requires
/// Turing (compute capability 7.5) or newer; Pascal (dev GTX 1070, 6.1)
/// doesn't support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuArch {
    PascalOrOlder,
    TuringOrNewer,
}

const TURING_COMPUTE_CAP: f32 = 7.5;

pub fn detect_gpu_arch() -> Result<GpuArch> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
        .output()
        .context("failed to run nvidia-smi (is the NVIDIA driver installed?)")?;
    if !output.status.success() {
        bail!("nvidia-smi exited with {}", output.status);
    }
    let text = String::from_utf8(output.stdout).context("nvidia-smi output was not UTF-8")?;
    parse_compute_cap(&text)
}

fn parse_compute_cap(text: &str) -> Result<GpuArch> {
    let first_line = text
        .lines()
        .next()
        .context("nvidia-smi returned no GPU info")?;
    let cap: f32 = first_line
        .trim()
        .parse()
        .with_context(|| format!("failed to parse compute capability from {first_line:?}"))?;
    Ok(if cap >= TURING_COMPUTE_CAP {
        GpuArch::TuringOrNewer
    } else {
        GpuArch::PascalOrOlder
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_compute_cap() {
        assert_eq!(parse_compute_cap("6.1\n").unwrap(), GpuArch::PascalOrOlder);
    }

    #[test]
    fn turing_and_ampere_compute_cap() {
        assert_eq!(parse_compute_cap("7.5\n").unwrap(), GpuArch::TuringOrNewer);
        assert_eq!(parse_compute_cap("8.6\n").unwrap(), GpuArch::TuringOrNewer);
    }

    #[test]
    fn malformed_output_errors() {
        assert!(parse_compute_cap("").is_err());
        assert!(parse_compute_cap("not a number\n").is_err());
    }
}
