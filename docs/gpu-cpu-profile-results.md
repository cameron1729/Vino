## Live GPU driver CPU profile

The 90-second desktop trial on 20 October 2026 completed successfully. The
worker exited normally, recovery reported no errors, and a subsequent X11 check
matched all three displays to the saved layout: laptop primary below the BenQ
and Dell, all at 60 Hz. The official DisplayLink service was active afterward.

The source run is **/var/tmp/vino-live-6d5_0obk**. Raw recordings, the exact worker,
kernel symbols, exported reports and the analysis script are archived under
**gpu-artifacts/cpu-profile-6d5_0obk/**. The [analysis JSON](gpu-cpu-profile-results.json)
contains the counters, sample weights, function breakdown and SHA-256 hashes.

### What used the CPU

These are shares of the driver's sampled CPU time, **not percentages of a CPU
core**. The window spans the first and last steady health records, elapsed
1.852–91.064 seconds, excluding most startup and shutdown activity.

| Work | Share of sampled driver CPU |
|---|---:|
| Kernel EVDI capture calls: pixel copies, cache flushing and buffer checks | 49.2% |
| Strip change detection/planning, predominantly image hashing | 17.4% |
| Converting captured XRGB pixels to RGB | 15.1% |
| Main-thread memory copies | 5.4% |
| Main-thread memory comparisons | 4.8% |
| Other work | 8.1% |

The process consumed 24.022 CPU seconds over the 89.212-second window, or
**26.9% of one logical CPU core**. The capture thread used 17.5% of one core
(13.2% kernel, 4.3% userspace); the main thread used 9.4%. Small differences
between process and thread totals reflect timer-accounting granularity.
This workload varied, with roughly 15–43% process CPU across ten-second windows.
It is not comparable as a before/after result to the previous 35.5% movement
window, and profiling itself adds overhead.

Within the kernel capture cost, sampled stacks account for approximately 28.1%
of all driver CPU in `_copy_to_user`, 12.6% in `drm_clflush_virt_range`, and 7.4%
in the copy-boundary checks under `__check_object_size`. In particular,
`find_vmap_area` belongs to those buffer checks; it is not classified as cache
flushing. These are inclusive stack figures, already contained in the first
table row, and must not be added to that row again.

The installed EVDI source's `copy_primary_pixels` loop flushes the source range
and calls `copy_to_user` for each damaged row. The loaded module's srcversion
matches the installed module, `12258322BFFCAAE41410871`. This identifies the
row-by-row transfer path as a concrete optimization candidate. It does not
establish that cache flushing can safely be removed: framebuffer ownership,
GPU/CPU coherence, offsets and source/destination strides still matter.

The annotated `HeadScanout::plan` instructions concentrate samples in the
multiply/rotate/load loop corresponding to `strip_hashes`. That confirms the
planner's large cost is substantially image hashing rather than only metadata
bookkeeping. The CPU profile does not measure GPU kernel execution time; a
thread sleeping for GPU or USB completion does not consume equivalent CPU time.

### Repeated work

The window contains 5,655 captures, averaging 63.4 per second. Of these, 2,026
(35.8%) were replaced by a newer snapshot in the mailbox before the presentation
thread consumed them. Capture and RGB conversion had already run for those
images. Another 1,809 consumed captures matched the previous image exactly.

The emitted frame records contain 1,821 updates with changed strips and 1,106
retransmission-only updates. All 2,280 frame records with a new capture reported
full-screen damage; this statement does not cover captures with no emitted
frame record. The 2,927 USB presentations average 32.8 per second and include
required repeats; this is not a measurement of monitor FPS.

Two restrictions remain important:

- Prompt EVDI grabs acknowledge compositor updates; throttling them previously
  caused dragging stutter, so a superseded image does not imply its entire grab
  can simply be omitted
- Keeping an owned snapshot may require a copy even if conversion moves later;
  the earlier native-XRGB-copy experiment was not faster than RGB conversion

### Recommended work

The largest CPU opportunity is reducing framebuffer transfer work in the EVDI
capture path. Investigate batching contiguous rows/ranges and repeated copy
checks while preserving dirty rectangles, strides, coherence and timely update
acknowledgements. This requires a scoped kernel/capture design and validation;
the 49.2% attribution is not a promise that all of it is removable.

The next contained change in the current userspace prototype is to skip
full-image hashing when resending a known-unchanged cached surface. There were
1,106 such retransmission-only presentations here. They must still be delivered
to satisfy the dock's buffering rules, but unchanged pixels need not be hashed
again. The 17.4% planner figure includes useful work on changing images too, so
it is an upper bound, not an expected saving from that change.

No codec, capture or kernel behavior was changed during this profiling task.
No new conclusion about power, heat or equal-output efficiency is drawn.

### Measurement checks and limits

The runner used the same native release worker as the last smooth idle trial,
SHA-256 `f544e88dcc9fe6af8b2fe9293eced169de96ac6c1f23565fc98b5e1039440d4e`,
with GPU encoding, independent capture, EVDI-composited cursor, 60-FPS cap and
Vulkan validation disabled. `perf` sampled user and kernel CPU with `cpu-clock`
at 199 Hz, 8 KiB DWARF stacks and monotonic timestamps. The report filters to
the worker PID, including its threads, and the common health-record window;
helper processes and startup are excluded from the table.

There are 4,753 retained samples, no reported lost samples and no unknown leaf
symbols. Their periods sum to 23.884 CPU seconds, within 0.6% of the process CPU
counter. The independent `perf report` and `perf script` exports agree on sample
count and total period. Some userspace stacks do not unwind beyond the sampled
function, so the generic memory-copy/comparison rows are not assigned entirely
to one caller. Function-level leaf attribution remains available for them.

The optional profiling runner passed 48 focused Python tests. Simulated wrapped
workers also exercised normal exit, early failure, watchdog termination and
runtime termination without opening display devices. The real recording's
preflight checked profiler access before display takeover, and its independent
recovery service restored the desktop afterward.
