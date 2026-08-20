use anyhow::{Context, Result};
use nvml_wrapper::Nvml;

/// NVENC feature set differs by GPU generation. `-temporal_aq` requires
/// Turing (compute capability 7.5) or newer; Pascal (dev GTX 1070, 6.1)
/// doesn't support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuArch {
    PascalOrOlder,
    TuringOrNewer,
}

const TURING_COMPUTE_CAP: (i32, i32) = (7, 5);

pub fn detect_gpu_arch() -> Result<GpuArch> {
    let nvml = Nvml::init().context("failed to load NVML (is the NVIDIA driver installed?)")?;
    let device = nvml
        .device_by_index(0)
        .context("failed to get GPU 0 from NVML")?;
    let cap = device
        .cuda_compute_capability()
        .context("failed to query CUDA compute capability")?;
    Ok(compute_cap_to_arch((cap.major, cap.minor)))
}

fn compute_cap_to_arch(cap: (i32, i32)) -> GpuArch {
    if cap >= TURING_COMPUTE_CAP {
        GpuArch::TuringOrNewer
    } else {
        GpuArch::PascalOrOlder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_compute_cap() {
        assert_eq!(compute_cap_to_arch((6, 1)), GpuArch::PascalOrOlder);
    }

    #[test]
    fn turing_and_ampere_compute_cap() {
        assert_eq!(compute_cap_to_arch((7, 5)), GpuArch::TuringOrNewer);
        assert_eq!(compute_cap_to_arch((8, 6)), GpuArch::TuringOrNewer);
    }
}
