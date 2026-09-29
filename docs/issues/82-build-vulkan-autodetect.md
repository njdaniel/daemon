# #82 — infra: `just build` includes Vulkan GPU whisper when the machine has the Vulkan packages

Labels: `size:S, model:sonnet, m2, lane:infra`

Proposed 2026-09-29, from testing #79 on the dev box. GPU whisper is a compile-time feature (`--features vulkan`), and `just build` builds without it, so a plain rebuild silently drops GPU support, and alternating the two builds recompiles whisper.cpp each time (about a minute). Decision taken with the user: one build command and one binary. `just build` detects the Vulkan packages and includes the feature when they are there. Where whisper runs stays a run-time choice (`[stt].device`, built-in default `cpu`; `--device gpu`).

## Goal
`just build` produces the most capable binary the machine can build: CPU + GPU (Vulkan) when `libvulkan-dev` and `glslc` are installed, CPU only otherwise, and says which.

## Size / Model / Turns
S | Sonnet | 30

## Scope
`justfile` (`build` recipe), README § Build and § Optional: whisper on the GPU, CLAUDE.md Commands. Out of scope: CUDA detection, changing the run-time default device, CI (it runs cargo directly and has no Vulkan packages).

- `just build` (variant `auto`): `--features vulkan` when `glslc` is on `PATH` and `pkg-config --exists vulkan`; otherwise the plain build.
- `just build cpu` forces the plain build; `just build gpu` forces Vulkan and fails loudly without the packages.
- The recipe ends by printing which it built, e.g. `built target/release/dmn: cpu + gpu (vulkan); whisper runs where [stt].device says (default cpu, --device gpu)`.

## Depends on
#61 (merged)

## Acceptance
- [ ] On a machine with `libvulkan-dev` and `glslc`, `just build` builds with Vulkan and says so; `dmn doctor` reports `built with vulkan`
- [ ] Without them, `just build` builds CPU-only and says so (and why)
- [ ] `just build cpu` and `just build gpu` force each variant
- [ ] The run-time default is unchanged: a Vulkan build with `[stt].device = "cpu"` transcribes on the CPU; `--device gpu` moves it to the GPU
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
just build && ./target/release/dmn doctor | grep -E "gpu backend|whisper device"
just build cpu && ./target/release/dmn doctor | grep "gpu backend"   # built without cuda/vulkan
just build                                                             # back to the Vulkan binary
```
