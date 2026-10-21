## Reusing hashes for unchanged retransmissions

The live trial now reuses the previous input's strip hashes when exact capture
comparison reports identical pixels, or when no new surface or cursor update
has arrived. Changed pixels still take the ordinary hashing path. Capture and
its EVDI acknowledgement continue independently of presentation.

The planner still sends every copy required by the dock's buffer rotation.
It hashes the first image and resized images, preserves forced keyframes,
and retains pending hashes after failed encoding or delivery so retries cannot
silently revert to an older image. Successful delivery alone commits the
content hashes and pays the retransmission debt. Existing callers of the
ordinary planning/presentation APIs continue to hash their input.

### Component measurements

Four alternating pairs compared the existing always-hash API with hash reuse
in the same native release build on 20 October 2026. Each sample used eight
warmup cycles and 256 measured cycles, with two required retransmissions per
cycle, on a padded 1920 x 1088 RGB surface. The table reports median thread CPU
time per retransmission across four samples, measured with
`CLOCK_THREAD_CPUTIME_ID`.

| Work measured | Always hash, CPU ms/repeat | Reuse hashes, CPU ms/repeat |
|---|---:|---:|
| Planning and committing a repeated update | 1.033 | 0.006 |
| Cached frame preparation, one changed strip | 1.037 | 0.011 |
| Cached frame preparation, 960 x 512 region | 1.467 | 0.205 |

Cached frame preparation includes planning, change reporting, assembling cached
strip bodies into wire records, and committing the ledger. The new image's
encoding happens before the timed interval; GPU encoding is unnecessary for
either path's repeats. Both paths produced the same total framed byte counts.
Correctness tests separately compare exact bytes, plans and presentation counts.

These measurements exclude capture, RGB conversion, exact capture comparison,
new-strip encoding, GPU execution, USB/control traffic and the compositor.
They demonstrate a saving in the targeted work, not a percentage reduction in
whole-driver CPU or laptop power. Fully settled idle already skipped planning,
so this change helps while the dock still owes repeated copies after an update.
The [previous profile](gpu-cpu-profile-results.md) found capture to be the
largest CPU consumer; that work is unaffected. No new claim about heat, fan
speed or battery life is established.

### Validation

All 56 Chimera tests and five GPU tests pass. Coverage includes identical
black first images, interleaved changes, required repeats, settled idle,
forced keyframes, changed geometry with the same strip count, failed encoder
batches and uncommitted deliveries followed by retries or replacement pixels.
The reference path rehashes every input and must produce identical wire bytes,
presentation counts, change bounds and remaining retransmission obligations.

Vulkan verification on the Radeon 780M passes for 160 surfaces, 16,520 strips
and 64 framed heads, with no validation errors. Stateful GPU checks now use
hash reuse on identical images and compare against the always-hash CPU path.
The native release build, strict GPU-package Clippy checks excluding dependency
lints, and read-only desktop preflight pass. The broader strict dependency
check encountered existing `manual_div_ceil` and `type_complexity` lints in
**vino-driver**; those files were not changed for this optimization.

The offline checks did not take over the monitors or official service. The
subsequent live trial passed; its observations and limits follow below.

### Live check

The 90-second trial **/var/tmp/vino-live-4965gf8p** completed normally with the expected
updated worker. The user reported “That felt really smooth and good”. Recovery logged
success with no errors. An independent X11 check matched all three monitors'
saved identities, positions, modes, refresh rates and primary flag; the official
DisplayLink service and compositor were active.

The first-to-last health interval spans 89.092 seconds.
It contains 2,895 frame records, including
1,106 required repeats of known-identical pixels.
The previous profile also contained 1,106 repeats.

| Observed live metric | Previous profile | Updated profile |
|---|---:|---:|
| Median repeated-frame preparation, elapsed ms | 1.555 | 0.123 |
| Median preparation for repeats of at most 4 KiB, elapsed ms | 1.553 | 0.042 |
| Process CPU, % of one logical core | 26.9 | 27.5 |
| Capture-thread CPU, % of one logical core | 17.5 | 19.1 |
| Main-thread CPU, % of one logical core | 9.4 | 8.4 |
| Captures per second | 63.4 | 66.9 |

Live preparation timings are elapsed wall time, excluding USB, whereas the
earlier component benchmark measures CPU time. Repeated payloads differed
(median 6,184 bytes previously and
32,056 bytes now), and capture/input
activity was not controlled. The much cheaper repeat path is visible on real
hardware and smoothness was retained, but the whole-driver CPU total is
approximately unchanged; these manual trials do not isolate an overall CPU
saving or establish a power/fan improvement.

Strip planning's share of sampled worker CPU fell from 17.4% to
9.6%. EVDI capture ioctls now account for
53.6%
and RGB conversion for 14.9%, together
68.5%. These are shares of worker CPU, not percentages of a core.
Framebuffer capture remains the largest next investigation target; this result
does not justify removing cache-coherence work or delaying EVDI acknowledgements.

The profile contains 4,838 samples, no reported lost
samples and no unknown leaf symbols. CPU-clock periods total
24.312 CPU seconds, within
0.63% of the process
counter. Some stacks do not unwind fully, so generic memory routines remain
separate from the caller-specific categories. Raw data, exact worker, exported
symbols/stacks, recovery checks and reproducible analysis are retained in
**gpu-artifacts/retransmit-hashes-cx2s3244/live-4965gf8p/**.

### Evidence and reproduction

The [measurement JSON](gpu-retransmit-hash-results.json) contains every sample,
validation results and source/binary fingerprints. Baseline and final workers,
source snapshots, verification output, the benchmark project and analysis
script are retained under **gpu-artifacts/retransmit-hashes-cx2s3244/**.
The benchmark uses no display devices; it can be rebuilt and run from the
project root:

```bash
RUSTFLAGS='-C target-cpu=native' CARGO_TARGET_DIR="$PWD/revdi/target" cargo build --release --locked --offline --manifest-path gpu-artifacts/retransmit-hashes-cx2s3244/benchmark/Cargo.toml
revdi/target/release/vino-retransmit-hash-bench
```
