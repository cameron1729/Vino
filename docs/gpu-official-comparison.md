## Official DisplayLink comparison

The installed DisplayLink service uses less CPU than Vino's earlier CPU-only
benchmark, and is the relevant baseline for replacing the current driver.
The GPU prototype uses less CPU than the live service in this first controlled
input comparison. It does not establish a faster or more efficient complete
DisplayLink replacement: output delivery and equivalent quality were not
measured, and the offline prototype omits capture and USB transport.

### Local setup and measurements

The test used the existing **DisplayLink 6.4.0-22** service with **EVDI
1.15.1-197**, KDE/X11, the Radeon 780M, and two 1920×1080 dock displays running
at 60 Hz. The USB dock is **17e9:4307**, connected at 5 Gbit/s. The driver remained
running throughout. No display modes, firmware, service settings, kernel
parameters, or power settings were changed.

Each active case presented the same deterministic 16-frame, two-head sequence
used by the offline benchmark. A native X11 MIT-SHM player uploaded the
exported BGRX8888 frames at the requested cadence. It verified 32 pixel samples
from its own windows before each run. Pixel checks establish input rendering,
not pixels decoded by the dock. Static baselines held one frame without
repeated image submissions.

There were three runs per condition, each with at least three seconds of warmup
and a requested ten-second input sequence. The offline GPU completes every
requested frame, extending its measurement window if it cannot keep up.
Warmup finished at a 16-frame boundary.
Official/GPU order alternated by repetition. A CPU value of 1.0 means one
logical processor's worth of CPU time; it does not mean a reserved physical
core. Values below are arithmetic means, with raw repetitions in the linked
JSON. These are exploratory measurements, not confidence-qualified acceptance
results.

| Workload | Requested input pairs/s | Official service CPU cores | Offline GPU process CPU cores | GPU completed pairs/s |
|---|---:|---:|---:|---:|
| Static dock displays | No updates | 0.246 | — | — |
| Scroll | 30 | 1.132 | 0.633 | 26.42 |
| Motion | 10 | 0.770 | 0.206 | 10.00 |
| Motion | 30 | 1.049 | 0.562 | 30.00 |

One pair contains an image for each display. The official player submitted the
requested input rate on average; that rate must not be read as the official
encoder's delivered FPS. X11 submission occasionally missed its intended
per-frame deadline and caught up afterwards; per-run counts and submission p99
are retained in the data. XSync confirms the X server handled requests, not
that the compositor, encoder, USB device or monitor completed them.

The 30-pairs/s scrolling case exceeds this GPU implementation's observed
capacity. Its lower CPU number therefore cannot establish equal-rate CPU
savings. Motion comparisons are more useful, especially the 10-pairs/s case,
but official frame dropping and quality changes remain unverified for every
case.

### What the CPU numbers include

The official measurement reads the service's cgroup CPU counters. It includes
all service threads, userspace encoding and management, and kernel time charged
to those threads, including EVDI/USB work. It excludes the player, Xorg and KWin;
their CPU use is measured separately in the data. Static service usage is shown
as context and is not subtracted from active runs. Activity on the laptop screen
and other background processes remained uncontrolled.

The GPU measurement reuses the existing offline benchmark path, including
padding, damage detection, cache processing, GPU transfer/synchronisation,
readback and record framing. It includes CPU time for all prototype threads but
omits desktop capture and USB transport. Its output quality and bytes match
Vino's reference encoder, not an established match to the official driver's
adaptive codec/quality settings. Source images match; compositor processing
and the actual captured encoder inputs were not verified. Some of the apparent
CPU difference may be consumed by missing live integration work.

Consequently the earlier “85% less CPU” result against Vino's own CPU encoder
must not be applied to the installed DisplayLink service. These measurements
support further investigation of CPU offload; they establish neither equal
visual quality, higher delivered FPS, lower input latency nor reduced heat.

### Reproduction and validation

Release Clippy, 24 Chimera tests and 5 prototype tests passed. The quick GPU
check passed 48 strip comparisons across 24 surfaces and 32 framed heads, with
zero Vulkan validation errors. All 12 live runs passed their 32 input pixel
checks. Fixed-rate CPU/GPU samples did equal work; invalid-rate, no-overwrite
and empty-replay checks passed. The C helper compiles with warnings as errors.

The **vino-gpu export** command writes the existing benchmark corpus without
running a GPU or accessing the dock. The **vino-gpu sample** command measures
one backend at a specified rate. Existing encoding code and shaders are
unchanged by these measurement additions.

From the repository root, build the release binary and player, then run the
comparison from the KDE/X11 session when both external monitors can be covered
briefly:

```bash
RUSTFLAGS='-C target-cpu=native' cargo build \
    --manifest-path revdi/Cargo.toml -p vino-gpu-encoder --release --locked
mkdir -p gpu-artifacts/official-comparison
cc -O2 -std=c11 -Wall -Wextra -Werror tools/gpu/live-display.c \
    -o gpu-artifacts/official-comparison/live-display -lX11 -lXext -lm
python3 tools/gpu/compare-official.py \
    --output gpu-artifacts/official-comparison/new-run
```

The runner requires a new output directory, detects the two dock monitor
rectangles, and aborts if geometry or process identity changes. Windows close
automatically, Escape stops a live replay, and each replay has a bounded
watchdog. It requires readable service CPU counters and X11 access; it does not
require sudo. The compiled C helper is intended to be launched by the Python
runner, which checks monitor geometry first.

[Measurement data](gpu-official-comparison.json) records binary/corpus hashes,
commands, setup, individual runs, input deadline misses and the separate player,
Xorg and KWin CPU measurements. Raw corpus files and per-run measurements remain
under **gpu-artifacts/official-comparison/run-1/**, including the preserved
**vino-gpu-measured**, **live-display-measured.c** and
**compare-official-measured.py** inputs and the **live-display-measured**
player binary. Later validation-only changes are not retroactively part of
those runs.

A stronger comparison needs verified capture and USB/output progress,
characterised output quality, and a working live GPU path. The installed EVDI
sysfs interface exposes configuration but no delivered-frame counter. Privileged
tracing could add capture/USB evidence; capture events alone would still not
prove presentation at the monitor. The
[official EVDI source](https://github.com/DisplayLink/evdi) describes the capture
interface, separate from the proprietary encoder and USB display pipeline.
