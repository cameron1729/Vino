## Initial GPU prototype results

This report records the original implementation. The later
[shared-memory follow-up](gpu-encoder-followup.md) measures the revised packer.

**Initial decision: this prototype does not meet the integration target.** It produces the same encoded bytes as the CPU reference, but does not deliver the required 2× throughput improvement or dual-1080p60 capacity. The power measurements do not establish an energy saving across both dense workloads.

This is a result for this implementation and test system. It does not show that a different GPU encoding architecture, or the Windows implementation, cannot perform better.

### System and method

Measured on 2026-10-16: AMD Ryzen 7 PRO 8840U w/ Radeon 780M Graphics, AMD Radeon 780M Graphics (RADV PHOENIX), Linux 7.0.0-34-generic. The installed driver is RADV/Mesa 25.2.8; the shader compiler is shaderc 2023.8 with glslang 14.0.0. Rust is pinned to 1.99.1, compiled in release mode with `-C target-cpu=native` (AVX2 and AVX-512F enabled for the CPU compiler).

Two 1920×1080 RGB888 heads, black padding to 1920×1088, the same 16-frame corpus and the same damage/cache/framing implementation were used for both backends. Each dense workload has five paired repetitions in alternating order, 15-second warmups, 5-second saturation windows and 60-second paced energy windows. Worker-count and energy sweeps selected CPU configurations independently. See [the measurement method](gpu-encoder.md#measurements) for the full scope and acceptance criteria.

The power sensor covers the AMD APU, including CPU and GPU/SoC. It does not measure the rest of the laptop, the dock or external displays. This was a desktop-session measurement with automatic clocks and background activity; it is not a controlled laboratory power measurement.

### Dense workload throughput

Frame-pair rates below are arithmetic means across the five saturation runs. The ratio is the paired geometric mean; its interval is a paired percentile bootstrap. GPU p99 is the worst per-run p99 across both saturation and paced measurements. One pair contains both heads.

| Workload | CPU workers | CPU pairs/s | GPU pairs/s | GPU/CPU ratio (95% CI) | Worst GPU p99 |
|---|---:|---:|---:|---|---:|
| motion | 8 | 30.71 | 29.49 | 0.962× (0.881–1.050) | 52.41 ms |
| noise | 16 | 18.68 | 9.88 | 0.534× (0.488–0.585) | 123.02 ms |

Neither workload meets the 60 frame-pairs/s and 16.7 ms p99 requirement. The CPU baseline is Vino's native userspace codec with measured parallelism; the official DisplayLinkManager was not benchmarked.

### Combined CPU/GPU energy

These are per-run averages from the paced windows. Every CPU/GPU pair completed identical frames, encoded strips and framed bytes. An equal-rate energy win also requires both backends to sustain the selected rate within 2%; equal work alone does not satisfy that condition.

| Workload | CPU workers | Target pairs/s | CPU package W | GPU package W | Raw GPU/CPU J per pair (95% CI) | All rates matched |
|---|---:|---:|---:|---:|---|---|
| motion | 16 | 27.44 | 17.72 | 17.63 | 0.915–1.002 | no |
| noise | 4 | 7.63 | 18.39 | 16.29 | 0.820–0.955 | yes |

For motion, the rate check failed (pair 1: CPU 24.17, GPU 27.44 pairs/s). Its raw energy interval cannot establish equal-rate energy savings.

The noise energy gate passed at 7.63 pairs/s. That saving was measured below the 60 Hz target and does not resolve the throughput or latency failure.

Process CPU use fell when work moved to the GPU:

| Workload | CPU backend average cores | GPU backend average cores |
|---|---:|---:|
| motion | 4.22 | 0.57 |
| noise | 1.80 | 0.21 |

A package-energy win requires a matched rate and a ratio interval entirely below 1; the table above reports both checks. These measurements do not establish lower laptop temperature or better battery life.

### GPU stage timings

These are aggregate milliseconds per frame pair across the GPU saturation runs. Transform and packing timestamps are contained within submission/wait time, so the columns must not be added together. Other host work includes padding, hashing, cache/framing work, query retrieval and scheduling.

| Workload | Upload | Transform | Packing | Submission/wait including GPU work | Readback | Other host work | Total wall |
|---|---:|---:|---:|---:|---:|---:|---:|
| motion | 0.78 | 3.15 | 6.07 | 12.77 | 1.06 | 19.29 | 33.91 |
| noise | 0.73 | 2.68 | 62.96 | 69.11 | 3.10 | 28.29 | 101.23 |

Packing is the large GPU cost on noisy input. The remaining host work also matters to end-to-end latency. Further performance work should profile packing memory access and lane utilisation, together with host copying, hashing and framing. These timings identify stages; they do not by themselves identify the instruction or memory bottleneck inside a shader.

### Desktop diagnostics

These shorter runs used one pair, 1-second warmups and saturation windows, and 5-second energy windows. They are behavioral checks and exploratory timings, not acceptance-grade power comparisons.

| Workload | GPU saturation pairs/s | GPU paced p99 | Measured GPU dispatches | Encoded strips |
|---|---:|---:|---:|---:|
| idle | 376.27 | 6.37 ms | 0 | 0 |
| small | 241.16 | 7.18 ms | 1200 | 600 |
| scroll | 15.60 | 77.28 ms | 268 | 273360 |

Settled idle submitted no GPU work. Small changes encoded only the affected strips; this implementation still uploads the full padded surface for a nonempty batch. Scrolling is a deterministic text-like synthetic image, not a captured terminal or browser session.

### Correctness and artifacts

Full hardware verification passed for 16,520 strips across 160 surfaces and 64 framed head outputs, with 0 Vulkan validation errors. The independent Ella grammar decoder accepted all 72 exported samples. The 24 Chimera unit tests, 5 prototype tests and 4 extractor checks passed, as did the prototype's Clippy checks.

[Compact measurement data](gpu-encoder-results.json) contains per-run results, worker sweeps, verification and measured-source fingerprints. Raw samples and checkpoints are in **gpu-artifacts/acceptance/** and **gpu-artifacts/desktop-diagnostics/**. The Windows payloads are separate and ignored. The [shader inventory](gpu-windows-shaders.md) now records 76 validated, disassembled compute containers after the follow-up corrected the extraction boundary.

Measured release binary SHA-256: `de727dc78cd466316786776203f3898167c927042f91903443d6eb797c515786`. The CPU reference is the unchanged vendored codec from base commit `490aa8e13488c9dbf0c88cb80aa4676d525a50ef`.

The final build also passed validation after adding startup and reporting safeguards; the encoder kernels and measured processing path are unchanged. Both measured and final binary fingerprints are recorded in the compact data.

The prototype has not been connected to the live dock or installed into the display stack. Keep live integration deferred while the performance target remains unmet.
