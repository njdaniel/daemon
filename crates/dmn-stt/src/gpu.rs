//! Which device whisper runs on (#61).
//!
//! whisper.cpp takes `use_gpu` and a `gpu_device` ordinal: it counts ggml's registered devices of
//! type GPU or integrated GPU, in registry order, and takes the N-th. When there is none it logs
//! "no GPU found" and quietly runs on the CPU, which would hide a misconfiguration. So `dmn`
//! enumerates the same registry itself, picks the device explicitly (a discrete GPU before an
//! integrated one: the dev box's Vulkan build sees both the RTX 4090 and the Ryzen iGPU), and
//! says where it ended up. The CPU is the fallback for every failure; see
//! [`Transcriber::load`](crate::Transcriber::load).

use std::ffi::{CStr, c_char};

use crate::transcriber::Backend;

/// Where whisper should run (`[stt].device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    /// Always the CPU.
    #[default]
    Cpu,
    /// The GPU; if there is none, or it fails, the CPU with a warning.
    Gpu,
    /// The GPU when this build has a GPU backend and a device is present, else the CPU, quietly.
    Auto,
}

/// One GPU as ggml's backend registry reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    /// whisper.cpp's `gpu_device`: the position among GPU and integrated-GPU devices.
    pub ordinal: i32,
    /// ggml's backend name, e.g. "CUDA0" or "Vulkan0".
    pub name: String,
    /// The driver's description, e.g. "NVIDIA GeForce RTX 4090".
    pub description: String,
    pub integrated: bool,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl GpuDevice {
    pub fn label(&self) -> String {
        format!(
            "{} ({}, {:.1} of {:.1} GB free)",
            self.description,
            self.name,
            gb(self.free_bytes),
            gb(self.total_bytes)
        )
    }
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

/// Where whisper ended up, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Gpu(GpuDevice),
    /// On the CPU; `reason` is `None` when the CPU was asked for, else why the GPU was not used.
    Cpu {
        reason: Option<String>,
    },
}

impl Placement {
    pub fn is_gpu(&self) -> bool {
        matches!(self, Placement::Gpu(_))
    }

    /// For the status line and `dmn doctor`: "GPU NVIDIA GeForce RTX 4090 (Vulkan0, …)",
    /// "CPU", or "CPU (fallback: …)".
    pub fn label(&self) -> String {
        match self {
            Placement::Gpu(d) => format!("GPU {}", d.label()),
            Placement::Cpu { reason: None } => "CPU".into(),
            Placement::Cpu { reason: Some(r) } => format!("CPU (fallback: {r})"),
        }
    }

    /// Asked for a GPU and did not get one.
    pub fn is_fallback(&self) -> bool {
        matches!(self, Placement::Cpu { reason: Some(_) })
    }
}

/// Every GPU and integrated GPU ggml registered, in whisper.cpp's `gpu_device` order. Empty in a
/// CPU-only build, where the registry holds only the CPU.
pub fn gpu_devices() -> Vec<GpuDevice> {
    let mut out = Vec::new();
    for dev in registry() {
        if dev.kind == DevKind::Gpu || dev.kind == DevKind::Igpu {
            out.push(GpuDevice {
                ordinal: i32::try_from(out.len()).unwrap_or(i32::MAX),
                name: dev.name,
                description: dev.description,
                integrated: dev.kind == DevKind::Igpu,
                free_bytes: dev.free,
                total_bytes: dev.total,
            });
        }
    }
    out
}

/// The device a GPU run should use: the discrete GPU with the most memory, else an integrated
/// one. Never trusts enumeration order alone.
pub fn pick(devices: &[GpuDevice]) -> Option<&GpuDevice> {
    devices
        .iter()
        .filter(|d| !d.integrated)
        .max_by_key(|d| d.total_bytes)
        .or_else(|| devices.iter().max_by_key(|d| d.total_bytes))
}

/// Resolve `choice` against what this binary and machine offer, before any model is loaded. A
/// GPU asked for and not available resolves to the CPU with the reason (a fallback the caller
/// warns about); `Auto` without a GPU is plain CPU, not a fallback.
pub fn resolve(choice: DeviceChoice, backend: Backend, devices: &[GpuDevice]) -> Placement {
    match choice {
        DeviceChoice::Cpu => Placement::Cpu { reason: None },
        DeviceChoice::Gpu | DeviceChoice::Auto => {
            let quiet = choice == DeviceChoice::Auto;
            if backend == Backend::Cpu {
                return cpu_because(
                    quiet,
                    "this dmn was built without a GPU backend (build with --features vulkan or cuda)",
                );
            }
            match pick(devices) {
                Some(d) => Placement::Gpu(d.clone()),
                None => cpu_because(quiet, &format!("{} found no GPU", backend.name())),
            }
        }
    }
}

/// VRAM one whisper context takes beyond its model file: KV caches and the compute buffers.
/// small.en measured 2026-09-26: kv self 18.9 + kv cross 56.6 + kv pad 4.7 + compute (conv 22.4,
/// encode 128.0, cross 6.2, decode 97.3) = 334 MB; rounded up for larger models and driver slack.
pub const STATE_BYTES: u64 = 448 * 1024 * 1024;
/// VRAM left free after whisper loads, so the desktop and the LLM keep working.
pub const VRAM_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

/// Whether one whisper context with a `model_bytes` model fits on `device` now. ggml aborts the
/// whole process when a GPU allocation fails (`GGML_ASSERT(buffer)` in ggml-backend.cpp), so an
/// out-of-memory GPU is never tried: this check is the fallback's first line, and it runs per
/// lane, against the free memory the registry reports at that moment.
pub fn fits(device: &GpuDevice, model_bytes: u64) -> Result<(), String> {
    let need = model_bytes + STATE_BYTES + VRAM_RESERVE_BYTES;
    if device.free_bytes >= need {
        Ok(())
    } else {
        Err(format!(
            "{} has {:.2} GB free, whisper needs {:.2} GB",
            device.name,
            gb(device.free_bytes),
            gb(need)
        ))
    }
}

fn cpu_because(quiet: bool, reason: &str) -> Placement {
    Placement::Cpu {
        reason: (!quiet).then(|| reason.to_owned()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevKind {
    Cpu,
    Gpu,
    Igpu,
    Other,
}

struct RegDev {
    kind: DevKind,
    name: String,
    description: String,
    free: u64,
    total: u64,
}

/// ggml's registered backend devices. whisper-rs exposes no safe wrapper for the registry, so
/// this reads it through `whisper-rs-sys`, the same calls whisper.cpp's own device selection
/// makes (`whisper_backend_init_gpu`).
#[allow(unsafe_code)] // CLAUDE.md allows unsafe in dmn-stt; this is its only use.
fn registry() -> Vec<RegDev> {
    use whisper_rs::whisper_rs_sys as sys;
    // SAFETY: `ggml_backend_dev_count` takes no arguments and only reads ggml's static backend
    // registry, which ggml initialises on first use and never frees.
    let n = unsafe { sys::ggml_backend_dev_count() };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: `i < ggml_backend_dev_count()`, so `ggml_backend_dev_get(i)` returns a valid,
        // registry-owned device handle that lives for the whole process.
        let dev = unsafe { sys::ggml_backend_dev_get(i) };
        if dev.is_null() {
            continue;
        }
        // SAFETY: `dev` is a valid device handle (above); `ggml_backend_dev_type` only reads it.
        let kind = match unsafe { sys::ggml_backend_dev_type(dev) } {
            sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_CPU => DevKind::Cpu,
            sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU => DevKind::Gpu,
            sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU => DevKind::Igpu,
            _ => DevKind::Other,
        };
        // SAFETY: both return NUL-terminated strings owned by the device, valid while it is,
        // i.e. for the process; `text` copies them before returning.
        let name = text(unsafe { sys::ggml_backend_dev_name(dev) });
        // SAFETY: as above.
        let description = text(unsafe { sys::ggml_backend_dev_description(dev) });
        let (mut free, mut total) = (0usize, 0usize);
        // SAFETY: `dev` is valid, and `free` / `total` are live `usize`s the call writes once.
        unsafe { sys::ggml_backend_dev_memory(dev, &mut free, &mut total) };
        out.push(RegDev {
            kind,
            name,
            description,
            free: free as u64,
            total: total as u64,
        });
    }
    out
}

/// Copy a C string the registry owns; empty when null.
#[allow(unsafe_code)] // CLAUDE.md allows unsafe in dmn-stt.
fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: callers pass a non-null (checked above), NUL-terminated string from ggml that
    // outlives this call; it is copied, never kept.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(ordinal: i32, name: &str, integrated: bool, total_gb: u64) -> GpuDevice {
        GpuDevice {
            ordinal,
            name: name.into(),
            description: name.into(),
            integrated,
            free_bytes: total_gb * 1_000_000_000 / 2,
            total_bytes: total_gb * 1_000_000_000,
        }
    }

    #[test]
    fn a_discrete_gpu_beats_an_integrated_one_whatever_the_order() {
        // The dev box's Vulkan build: the Ryzen iGPU could be enumerated first on another driver.
        let igpu = dev(0, "RADV", true, 32);
        let rtx = dev(1, "RTX 4090", false, 24);
        let devices = [igpu, rtx.clone()];
        assert_eq!(pick(&devices), Some(&rtx));
        let only_igpu = [dev(0, "RADV", true, 32)];
        assert_eq!(pick(&only_igpu).map(|d| d.ordinal), Some(0));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn resolve_falls_back_to_the_cpu_and_says_why() {
        let rtx = [dev(0, "RTX 4090", false, 24)];
        assert_eq!(
            resolve(DeviceChoice::Cpu, Backend::Vulkan, &rtx),
            Placement::Cpu { reason: None }
        );
        assert!(resolve(DeviceChoice::Gpu, Backend::Vulkan, &rtx).is_gpu());
        assert!(resolve(DeviceChoice::Auto, Backend::Cuda, &rtx).is_gpu());

        let built_without = resolve(DeviceChoice::Gpu, Backend::Cpu, &rtx);
        assert!(built_without.is_fallback(), "{built_without:?}");
        assert!(built_without.label().contains("without a GPU backend"));

        let none_found = resolve(DeviceChoice::Gpu, Backend::Cuda, &[]);
        assert!(none_found.is_fallback());
        assert!(
            none_found.label().contains("CUDA found no GPU"),
            "{}",
            none_found.label()
        );

        assert_eq!(
            resolve(DeviceChoice::Auto, Backend::Cuda, &[]),
            Placement::Cpu { reason: None },
            "auto without a GPU is plain CPU, not a warning"
        );
    }

    #[test]
    fn a_model_is_only_put_on_a_gpu_with_room_for_it() {
        let small_en = 487_614_201;
        let mut rtx = dev(0, "Vulkan0", false, 24);
        // Beside Ollama's 65k-context model the 4090 had 0.8 GB free: small.en does not fit.
        rtx.free_bytes = 800_000_000;
        let err = fits(&rtx, small_en).unwrap_err();
        assert!(err.contains("Vulkan0 has 0.80 GB free"), "{err}");
        rtx.free_bytes = 2_000_000_000;
        assert!(fits(&rtx, small_en).is_ok());
    }

    #[test]
    fn a_cpu_only_build_registers_no_gpu() {
        if Backend::compiled() == Backend::Cpu {
            assert!(gpu_devices().is_empty(), "{:?}", gpu_devices());
        }
    }
}
