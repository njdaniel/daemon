//! #61: where whisper actually runs, through `Transcriber::load` with a real model. Needs
//! `small.en` under `~/.cache/dmn/models/` (`dmn doctor --fetch small.en`), so it is ignored in
//! CI; `just probe` runs it. In a CPU-only build `device = gpu` must fall back with the reason; in
//! a `--features vulkan|cuda` build on a machine with a GPU it must land on the GPU (or, when the
//! GPU is out of memory, fall back rather than fail).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use dmn_stt::{Backend, DeviceChoice, Model, Placement, SttConfig, Transcriber, model_path};

/// `~/.cache/dmn/models/small.en`, as `dmn doctor --fetch small.en` puts it (dmn-stt does not
/// depend on dmn-core's path logic; the pipeline test resolves it the same way).
fn model() -> std::path::PathBuf {
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache"),
    };
    model_path(&cache.join("dmn/models"), Model::SmallEn)
}

fn load(device: DeviceChoice) -> Transcriber {
    Transcriber::load(
        &model(),
        SttConfig {
            threads: 4,
            device,
            ..SttConfig::default()
        },
    )
    .expect("a GPU problem falls back to the CPU; it never fails the load")
}

#[test]
#[ignore]
fn asking_for_the_gpu_lands_on_it_or_falls_back_with_a_reason() {
    let t = load(DeviceChoice::Gpu);
    let p = t.placement();
    eprintln!("device = gpu → {}", p.label());
    match Backend::compiled() {
        Backend::Cpu => {
            assert!(p.is_fallback(), "{p:?}");
            assert!(p.label().contains("without a GPU backend"), "{}", p.label());
        }
        Backend::Cuda | Backend::Vulkan => assert!(
            p.is_gpu() || p.is_fallback(),
            "a GPU build either uses the GPU or says why not: {p:?}"
        ),
    }
}

#[test]
#[ignore]
fn cpu_is_the_cpu_and_auto_never_warns() {
    assert_eq!(
        load(DeviceChoice::Cpu).placement(),
        &Placement::Cpu { reason: None }
    );
    let auto = load(DeviceChoice::Auto);
    assert!(!auto.placement().is_fallback(), "{:?}", auto.placement());
}
