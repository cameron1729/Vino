## GPU encoder shared-memory follow-up

Linux-only analysis of the Windows DLL identified relevant DL3 shader families
and prompted a change to Vino's GPU memory access. The revised packer passes the
existing correctness checks and substantially improves noisy-input throughput
over the original GPU implementation. It still does not meet the dual-1080p60
integration target.

### What changed

The [Windows investigation](gpu-windows-shaders.md#linux-host-tracing) recovered
58 shader names and host registrations, including the plain
`dl3_uncompacted` implementation. That shader keeps intermediate coefficients
in workgroup shared memory while constructing the compressed output.

Vino's packer previously read coefficients directly from device memory in both
the length and output loops. Adjacent packing lanes read different planes,
64 words apart. The revised shader first loads each plane cooperatively with
adjacent lanes reading adjacent words, then transposes the data into a shared
array. Both packing loops reuse that array. A 49-word stride spreads the two
access directions across shared-memory banks.

This is an independently implemented memory-layout change, informed by the
vendor architecture. The colour transform, quantisation, codewords, output
format and CPU codec are unchanged. No vendor shader is shipped or executed.

The device check now includes the declared 13,452-byte shared-memory
requirement. On this RADV build the compiled packer reserves 13,824 LDS bytes,
up from 1,024, and its reported subgroup capacity falls from 32 to 6. Neither
version spills registers. These are compiler statistics from
[Mesa's diagnostic interface](https://docs.mesa3d.org/envvars.html#radv-driver-environment-variables),
not hardware-counter measurements. The speedup below is measured independently.

### Original versus revised GPU

The system was the AMD Ryzen 7 PRO 8840U with integrated Radeon 780M,
RADV/Mesa 25.2.8 and Linux 7.0.0-34-generic. Rust 1.99.1 release builds used
`-C target-cpu=native` for both CPU and GPU backends.

Five alternating original/revised pairs used the same two 1920×1080 surfaces,
padding, corpus seed, damage/cache planner and record framing. Each run used
2-second warmups and a 1-second saturation window. These are short exploratory
comparisons, with five-second paced diagnostics excluded from this table.

Rates are arithmetic means; the ratio and interval use paired log ratios with
10,000 percentile-bootstrap samples. Packing times are frame-count-weighted.
One frame pair contains both display heads.

| Workload | Original GPU pairs/s | Revised GPU pairs/s | Paired speedup (95% CI) | Original packing ms | Revised packing ms |
|---|---:|---:|---|---:|---:|
| motion | 29.66 | 30.12 | 1.016× (0.968–1.065) | 5.65 | 6.08 |
| noise | 9.32 | 24.85 | 2.656× (2.497–2.758) | 65.26 | 7.89 |

Motion shows no established end-to-end gain. Its packing stage is slightly
slower after staging. Noise improves in all five pairs; the longer comparison
below checks the revised implementation against the CPU.

### Revised GPU versus a fresh CPU sweep

The subsequent run used five alternating CPU/GPU pairs, 3-second warmups and
5-second saturation windows. The native CPU backend swept 1, 2, 4, 8 and 16
workers and selected the fastest observed configuration for each workload.
The machine remained a normal desktop session with automatic clocks.

| Workload | CPU workers | CPU pairs/s | Revised GPU pairs/s | GPU/CPU ratio (95% CI) | Worst GPU p99 |
|---|---:|---:|---:|---|---:|
| motion | 16 | 31.07 | 30.11 | 0.975× (0.851–1.073) | 52.84 ms |
| noise | 8 | 16.18 | 26.23 | 1.626× (1.526–1.730) | 46.02 ms |

The p99 column is the worst per-run value across saturation and short paced
measurements. These are Vino CPU comparisons; the official Windows or Linux
DisplayLink encoder was not benchmarked. Changing clocks and background work
limit comparisons between separate runs, so the paired measurements carry
more weight than comparing absolute numbers with the older report.

Neither workload reaches 60 frame pairs/s with a 16.7 ms p99. The GPU packing
improvement does not remove host padding, hashing, cache/framing, copying and
synchronous submission costs. The new default remains an offline prototype.

The five-second paced samples in this follow-up are diagnostic only.
**There is no new claim of reduced package energy, laptop heat or battery use.**
The [original energy results](gpu-encoder-results.md) apply to the original
implementation; a new power conclusion requires the longer matched-rate
protocol.

### Validation and reproduction

The revised implementation passed 16,520 exact strip comparisons across 160
surfaces, 64 framed head comparisons and zero Vulkan validation errors.
The independent grammar decoder accepted all 72 exported samples. The
24 Chimera tests, 5 prototype tests, 4 extraction tests, 11 host-trace tests
and release Clippy check passed.

From the repository root:

```bash
RUSTFLAGS='-C target-cpu=native' cargo build \
    --manifest-path revdi/Cargo.toml -p vino-gpu-encoder --release --locked
revdi/target/release/vino-gpu verify --output gpu-artifacts/followup-verification
revdi/target/release/vino-gpu bench \
    --cases motion,noise --seconds 5 --warmup 3 --pairs 5 --sweep-seconds 5 \
    --output gpu-artifacts/followup-comparison
```

[Measurement data](gpu-encoder-followup.json) records the paired runs, CPU sweep,
validation and source/binary hashes. Full raw data, the preserved original
binary/shaders and compiler diagnostics remain in
**gpu-artifacts/windows-followup/**. The original/revised comparison used a
preserved candidate binary before the device-limit check was added; its shader
is byte-for-byte identical to the final shader. The fresh CPU comparison and
final verification used the final binary.

### Remaining research

The host trace maps registrations and one split-transform/packing branch. It
does not establish the runtime path Windows chooses for the local Ella dock.
The next useful binary work is to recover the provider-selection flags and
constant buffers for that branch, enabling an isolated Linux shader replay.
That remains separate from the measured memory-layout improvement above.
