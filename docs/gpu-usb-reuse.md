## USB packet and transfer reuse

The userspace presentation path now retains its large packet buffers and eight
libusb OUT transfer objects between frames. This applies to both CPU and GPU
encoding, including copied and direct capture.

The session assembles prefix, opener, report, image records and trailer directly
into retained 64-KiB chunks. It no longer allocates a complete intermediate stream
and copies that stream into fresh chunks. The active slice excludes unused chunks
from larger previous frames, while capacity stays available until session close.
Every presentation still gets its own sequence/trailer, and Ella's aligned-frame
16-byte tail split preserves the previous USB transfer boundaries.

The USB pool retains eight transfer descriptors and one bounded completion queue.
Successful frames no longer allocate/free these objects or cancel idle transfers.
A per-call guard drains outstanding completions or cancellations before clearing
borrowed buffer pointers, including on timeout, short write, disconnect, submission
failure or Rust unwinding. The OUT lock still serializes video and control writes.
Both async pools drop before the device handle and libusb context.

Small header/trailer allocations remain. This removes an extra full-frame copy,
repeated large packet allocations and transfer-object allocations; it does not
remove the kernel's USB copy or every userspace allocation.

### Validation and component results

All 73 Chimera library tests and 15 USB driver tests passed. Coverage includes
byte-for-byte and per-transfer agreement with the original assembler across
supported dock profiles, opening frames, fragmented records, wire-counter wrap,
head changes, and large/small/large reuse without stale bytes.

The USB tests simulate partial allocation and submission failures, completion
errors, delayed cancellation, callbacks on another thread or before submission
returns, a full completion queue, reuse after failure, and destruction/unwinding.
No USB device is opened by these tests.

The release component benchmark uses prebuilt fragmented payloads, eight warmup
iterations per variant and three alternating old/new pairs:

| Payload per presentation | Previous assembly, wall ms | Reused assembly, wall ms |
|---|---:|---:|
| 4,096 bytes | 0.000186 | 0.000121 |
| 65,536 bytes | 0.002326 | 0.001209 |
| 3,300,000 bytes | 0.233459 | 0.108997 |

For the 3.3-MB case, thread CPU fell from **0.2344 to 0.1107 ms per presentation**
(53% less), consistent with wall time. CPU ticks have 10-ms resolution across
each timed batch, so small-payload CPU percentages are not useful.

This is packet assembly in private RAM only. It excludes encoding, EVDI, the
compositor and USB transport, and does not measure the transfer-pool saving or
establish total live CPU/power improvement. It does not establish whether the
earlier scrolling kernel-CPU increase is resolved.

The optimized worker built successfully and the direct-capture preflight passed
on the Radeon 780M with four mapped capture slots and the 128-row EVDI candidate
available. The preflight changed no hardware. A physical trial of these changes
subsequently completed successfully, as recorded below.

Raw measurements, parsed results, build identity and the previous worker are in
**gpu-artifacts/usb-reuse-20261021/**.
The [packet benchmark](results/usb-reuse-20261021/packet-benchmark.json) is
preserved with the source.

### Completed live trial

The two-minute profiled desktop trial **/var/tmp/vino-live-p0v63e_b** ran for
120.016 seconds and exited successfully. The user reported that it looked fine.
Recovery had no errors, reset EVDI batching to zero and restored the original
layout. An independent check matched all three monitor positions and refresh
rates, confirmed the compositor was active and found the official driver running.
The recorded worker hash matches the optimized build.

There were 5,538 captures, 5,106 submitted updates and 2,849 GPU dispatches, all
using direct capture. Desktop uploads, RGB snapshots/conversion, repair bytes and
pool exhaustion remained zero. The 491,520 cursor-upload bytes are expected for
separate cursor updates. The worker transmitted 230.56 MB without a reported
transport failure.

Between the first and last health samples, the worker used **14.94% of one CPU
core**: main thread 9.68% and capture thread 5.25%. Main-thread kernel CPU was
0.52% of a core. These are observations from this manual workload, not a matched
before/after saving; activity and capture volume differed from previous trials,
and profiling adds overhead. The earlier high-traffic synthetic scrolling
regression was not reproduced by this workload.

The profile contains 3,509 samples, no reported lost samples and no unknown leaf
symbols. Sample periods account for 17.633 CPU seconds, within 1.2% of the process
counter's 17.846 seconds over the same interval:

| Work | Share of sampled driver CPU |
|---|---:|
| Strip change detection and hashing | 33.74% |
| Exact native-image comparison | 22.26% |
| EVDI capture ioctls, including copies and cache flushing | 32.94% |
| Userspace memory copies | 1.05% |
| Other CPU work | 10.00% |

These are shares of the driver's CPU, not percentages of a CPU core. Some caller
stacks do not fully unwind; generic memory-copy samples are not all assigned to
packet assembly. USB handling is not a dominant sampled cost in this run.

The next concrete code lead is native change detection. The consumer performs an
exact comparison against the prior capture, then any detected pixel or cursor
change causes the planner to hash every strip. Equality exits on the first
difference, so this is not invariably two complete screen scans. Comparing exact
pixels per strip and retaining content revisions for unchanged strips could
replace the separate duplicate check and full-screen hashing. Such a change must
preserve immutable input ownership, cursor footprints, retransmission debt and
pending versus committed state after failures. Exact comparison would remain;
the combined 56% sample share is not a forecast saving.

That change is now implemented in [exact native change detection](gpu-native-revisions.md).

The archived **live-p0v63e_b/** subdirectory contains the profile, exact worker,
kernel symbols, exported reports, reproducible **analyze-profile.py**, independent
**trial-summary.json** and **desktop-recovery-check.json**.
The published [profile summary](results/usb-reuse-20261021/profile-summary.json)
and [trial summary](results/usb-reuse-20261021/trial-summary.json) preserve the
measurements and their limitations; raw profiles and binaries remain local.

### Live check

1. Run the two-minute trial in a terminal, drag and scroll on the Dell, play video and **verify** smooth output and correct cursor movement:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run \
        --encoder gpu --source desktop --capture direct --cursor client \
        --evdi-copy-rows 128 --fps 60 --duration 120 --profile-cpu
    ```
2. Let it finish and **verify** both desktops return with `restored: true` and no recovery errors

The existing runner checks profiler access before display takeover, records CPU
stacks for subsequent investigation and restores the original layout and EVDI
batch setting automatically. Ctrl+C starts restoration early. Profiling adds
overhead, so this is a functional check and CPU diagnosis, not an unprofiled
comparison against the proprietary driver.

The hardware-free assembly benchmark can be repeated with:

```bash
RUSTFLAGS='-C target-cpu=native' cargo test --manifest-path revdi/Cargo.toml \
    -p vino-chimera --features live,revdi --offline --locked --release --lib \
    session::delivery_tests::packet_assembly_component_benchmark \
    -- --ignored --nocapture
```
