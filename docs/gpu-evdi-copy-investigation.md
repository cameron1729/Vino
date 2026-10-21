## EVDI framebuffer copy batching

The next optimization target is the copy from EVDI's framebuffer into the
capture buffer. In the last smooth live trial, capture ioctls consumed 53.6%
of sampled worker CPU. The worker used 27.5% of one logical core overall;
53.6% is a share of that work, not a percentage of a core. The measurements
and their limitations are in [the previous report](gpu-retransmit-hash-results.md).

The installed EVDI implementation flushes and copies each damaged row
separately. A full 1920 x 1080 capture therefore makes 1,080 cache-flush calls
and 1,080 `copy_to_user` calls. The prototype combines adjacent rows when
the source and destination contain packed, complete rows. At 32 rows per
batch, the same image uses 34 calls of each kind. The byte count stays the same.

The private-RAM kernel benchmark shows roughly 69–70% less CPU time in the
copy/cache-flush component with 32-row and 128-row batches. The first
90-second hardware trial with 128-row batching also completed successfully:
the user reported good visual behavior, and the recorded desktop state was
restored. Its profile estimates 29% less capture-ioctl CPU time per capture
than the previous manual trial. A temporary candidate module was loaded;
the installed module remains unchanged. Matched performance testing, power
measurements and broader hardware validation remain outstanding.

### Evidence that the path matters

The archived trial **/var/tmp/vino-live-4965gf8p** has 2,555 recorded captures
in its measured interval; all reported full-screen damage. This is the
subset associated with frame records, not every capture acknowledged by
the independent capture thread. Read-only DRM queries on 20 October 2026
also found both currently active EVDI framebuffers to be 1920 x 1080 XR24,
with a 7,680-byte pitch, zero offset and linear modifier. Those metadata
queries describe the current official desktop, not the earlier trial.
The prototype checks eligibility on every rectangle regardless.

The source used is **/usr/src/evdi-1.15.1**. Rebuilding it for
**7.0.0-34-generic** produces the same source version as the loaded module:
`12258322BFFCAAE41410871`. The source hash also matches the archived source
from the earlier CPU investigation. Source copies, fingerprints, build logs,
host tests and generated modules are retained in
**gpu-artifacts/evdi-copy-20261020/**.

### Prototype behavior

The [patch](../tools/gpu/evdi-copy-batching.patch) adds an experimental
`copy_batch_rows` module parameter. Its default is zero; zero and one retain
the existing row loop. Larger values cap the number of contiguous rows
copied together. A full-width band with a nonzero starting row also qualifies.

Batching requires a nonempty rectangle beginning at x = 0, ending at the
capture width, and identical source pitch, destination stride and row byte
count. Partial-width rectangles and either kind of row padding retain the
existing path. The existing Intel Xe path also remains separate. Checked
multiplication guards the combined byte count.

The patch retains the existing framebuffer reference, mapping, dimension
checks, CPU-access bracketing, cursor order and vblank acknowledgement. It
keeps cache flushing before the corresponding copy and continues to return
`-EFAULT` when the user copy fails. It does not add capture throttling or work
under the capture mailbox lock.

The AMD import path in this installed EVDI source maps pinned pages with
`PAGE_KERNEL`. That alone does not establish that flushing can be removed.
The kernel's [DMA-buffer documentation](https://docs.kernel.org/driver-api/dma-buf.html)
describes the CPU-access synchronization and cache-coherence obligations;
this experiment preserves the existing begin/end access calls and flushing.
The [upstream cache helper](https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_cache.c)
also shows per-call barriers around the x86 range flush. That moving upstream
source is explanatory, not a claim about the exact installed implementation:
the benchmark calls the helper exported by the running kernel.

### Completed validation

The original and patched EVDI modules and the separate benchmark module all
build for the running kernel. The build reports the missing optional
**pahole/vmlinux** information and skips module BTF generation; it succeeds
without it. No module installation, DKMS replacement or initramfs update is
part of preparation.

The 571 host comparison cases pass with AddressSanitizer,
UndefinedBehaviorSanitizer and leak detection. They compare entire destination
allocations, including guards and padding, and verify that successful copies
flush the same source bytes before reading them. Cases cover disabled batching,
multiple batch sizes, full frames, bands, partial and overlapping rectangles,
source offsets, independent padded strides, empty input, dimension errors,
modeled copy faults and delegation to the existing Xe routine. Five hundred
cases use reproducible randomized geometry. These tests model the memory
hooks; they do not establish GPU cache coherence on hardware.

Nine runner tests pass, including cleanup after success, client failure,
timeout and cancellation; refusal to take ownership of an existing module;
rejection of wrong kernels, module identities or hashes; and detection of
missing or duplicated result pairs. Assembly inspection confirms that both
benchmark routines retain calls to `__check_object_size`, `_copy_to_user`
and `drm_clflush_virt_range`.

The source extraction changes only test type names, the function name and a
compiler attribute preventing interprocedural constant folding. Without that
attribute, the compiler can specialize the synthetic fixed geometry and
remove buffer checks from the old routine, biasing the comparison. Production
EVDI receives its geometry at runtime. Extracted functions and assembly are
archived alongside the source snapshots.

### Component measurement

The [runner](../tools/gpu/bench-evdi-copy.py) loads only the separately named
**vino_evdi_copy_bench** module, with a root-only misc device. It allocates a
private 1920 x 1080 source buffer and copies into a private userspace mapping.
It never opens a DRM device, changes a mode, replaces EVDI or stops the
DisplayLink service. The loader validates the built module's identity,
kernel release and hash, stages it in a root-owned directory, and removes
that module when the bounded client finishes or fails. Ctrl+C also triggers
cleanup. Logs include before/after EVDI and service state.

The client checks real `EFAULT` responses for null pointers and an inaccessible
destination page, then compares the original routine against disabled
batching and batches of 8, 32, 128 and 1,080 rows. Each setting has four pairs
with alternating execution order, eight warmup frames and 64 measured frames
per sample. Only the client is pinned to one allowed logical CPU. Destination
prefaulting and byte verification happen outside the timed intervals.
Every measured sample records thread CPU time, elapsed time and kernel
elapsed time; CPU savings are summarized as paired reductions.

This measures the copy component with real kernel checks and flushing. The
source is ordinary private `vmalloc` RAM, not an imported GPU framebuffer.
It does not measure GPU synchronization, capture scheduling, RGB conversion,
USB, power, or the fan. Results can identify a promising batch size and reject
an unhelpful approach; a percentage improvement here is not a whole-driver
CPU saving. The disabled control helps expose systematic benchmark bias.

1. Leave the desktop mostly idle and run the prepared benchmark from a terminal where sudo can request your password:
    ```bash
    sudo python3 /home/cameronball/Projects/Vino/tools/gpu/bench-evdi-copy.py run --build /home/cameronball/Projects/Vino/gpu-artifacts/evdi-copy-20261020
    ```
2. **Verify** the command prints five comparison rows and reports that the benchmark module was removed
3. Retain the printed **/var/tmp/vino-evdi-copy-…** directory for analysis and **verify** the monitors continue showing the normal desktop

The client has a 90-second timeout. It requires no window dragging or display
takeover. Secure Boot is currently disabled on this laptop; preparation does
not change boot policy. The module is experimental and unsigned. Forced
termination of the root supervisor itself can leave the passive benchmark
module loaded; subsequent runs refuse to take ownership of it and report the
condition rather than replacing it.

### Measured component result

The completed run **/var/tmp/vino-evdi-copy-gotld692** contains 40 measured
samples, totaling 2,560 measured frames and 320 warmup frames. All 20 real
user-copy fault checks passed, as did the warmup and measured destination-byte
checks. The build report matches the prepared artifact. The original EVDI
source version and active official service match before and after the run;
the benchmark module was removed with no cleanup errors.

| Rows per batch | Copy calls/frame | Original CPU ms/frame | Candidate CPU ms/frame | Median paired CPU saving | Range across four pairs |
|---|---:|---:|---:|---:|---:|
| Disabled control | 1,080 | 1.099 | 1.098 | -3.0% | -5.2% to +5.0% |
| 8 | 135 | 1.099 | 0.443 | 59.9% | 55.7–62.3% |
| 32 | 34 | 1.094 | 0.347 | 68.6% | 66.1–69.0% |
| 128 | 9 | 1.104 | 0.330 | 69.8% | 67.2–70.2% |
| 1,080 | 1 | 1.131 | 0.374 | 67.2% | 61.7–68.9% |

The time columns are separate medians; the percentage column is the median
of each matched pair's percentage saving. Those summaries need not have the
same arithmetic relationship, which explains the nearly identical disabled
medians alongside its -3.0% paired result. The disabled control varies in both
directions by about 5%, whereas every enabled pair saves more than 55%.
Four alternating pairs support a substantial component improvement in this
run, without establishing a confidence interval or a globally optimal size.

The 128-row setting has the lowest median CPU time, with 32 rows close behind.
Their individual sample ranges overlap; the roughly 0.017 ms median difference
is small compared with the gain over the original loop. Copying the entire
frame in one batch was not fastest here. Both 32 and 128 rows are credible
live-test candidates, with 128 rows the initial measurement lead and 32 rows
worth retaining for comparison.

For scale only, saving about 0.75 ms per copy at 60 copies per second would
save about 45 ms of CPU time each second, or 4.5 percentage points of one
logical core. This is conditional arithmetic using the synthetic result and
an assumed rate, not an observed whole-driver improvement. The earlier 53.6%
profile category includes the complete capture ioctl; multiplying all of it
by this benchmark's saving would overstate what was measured. Power and fan
behavior were not measured in this run.

Raw results, cleanup/state records, the build report, independently checked
summary and file hashes are archived under
**gpu-artifacts/evdi-copy-20261020/component-gotld692/**. The
[investigation metadata](gpu-evdi-copy-investigation.json) includes every sample
and the paired ranges.

### First hardware result

The [temporary-module procedure](gpu-evdi-live-trial.md) successfully loaded
candidate source version `9232839421842403DFD2636` with batching initially
disabled. The saved layout matched all three monitors after login. Trial
**/var/tmp/vino-live-n7g38clw** then ran for 90 seconds using 128-row batching,
the unchanged GPU worker, independent EVDI capture, EVDI cursor composition
and a 60-FPS cap. The compositor remained active. The user reported
“K done, felt good”.

The worker exited normally with no error. Recovery recorded
`evdi_copy_rows_restored: 0`, and a separate check confirmed that batching
was disabled, the official DisplayLink service and compositor were active,
and all three monitors matched their saved identities, geometry, refresh
rates and primary flag. No trial or recovery unit remained active. The
installed module's SHA-256 still matches the original; no DKMS installation
or initramfs update took place.

The table compares the first-to-last health intervals of this trial and the
previous smooth trial **/var/tmp/vino-live-4965gf8p**. Both used the same worker
binary and trial configuration, but desktop activity was manual and differed.

| Observed metric | Previous original module | Candidate, 128 rows |
|---|---:|---:|
| Measurement interval, seconds | 89.092 | 89.278 |
| Worker CPU, % of one logical core | 27.46 | 21.87 |
| Capture-thread CPU, % of one logical core | 19.08 | 13.55 |
| Main-thread CPU, % of one logical core | 8.38 | 8.31 |
| Captures per second | 66.93 | 58.12 |
| Estimated capture-ioctl CPU ms/capture | 2.187 | 1.549 |
| Capture-thread kernel CPU ms/capture | 2.209 | 1.634 |

The observed worker total is 20.4% lower, but capture frequency is also 13.2%
lower and wire traffic differs substantially. The total is not an isolated
measurement of the batching gain. Dividing sampled capture-ioctl CPU periods
by acknowledged captures gives an estimated 29.2% lower cost per capture;
capture-thread kernel counters independently give a 26.0% lower cost per
capture. These normalized observations support the optimization, while still
being subject to sampling and workload differences.

Within capture stacks, estimated cache-flush CPU time per capture fell from
0.481 to 0.169 ms and buffer-check time from 0.249 to 0.017 ms. The
`_copy_to_user` cost changed much less, from 1.404 to 1.323 ms. This is
consistent with removing repeated call/check/barrier overhead while retaining
the pixel copy. These are inclusive stack estimates, not direct per-call
timers, and must not be added to the capture category. The synthetic private
RAM benchmark's roughly 70% component result does not describe the complete
live capture ioctl.

The remaining sampled worker CPU is 41.5% capture ioctls, 19.1% RGB conversion,
15.2% strip planning, 9.8% memory comparisons, 6.5% memory copies and 7.9%
other work. The profile contains 3,852 samples with no reported lost samples
or unknown leaf symbols. Sampled periods total 19.357 CPU seconds, within
0.85% of the process counter. All 2,770 captures associated with emitted frame
records reported full-screen damage; this subset does not include every
capture acknowledged by the independent thread.

The short hardware trial establishes successful operation and reported
smoothness on this setup, not pixel-by-pixel validation, long-term stability,
or a power/fan improvement. The next measurement should compare disabled and
enabled batching on the same temporary candidate under matched replay.
The independent capture acknowledgements must remain unchanged. This
prototype is not ready for production installation.

The temporary candidate remains loaded with batching disabled. An ordinary
reboot loads the unchanged installed module. Raw profile data, exact worker,
module-switch records, desktop recovery checks, perf exports, hashes and
reproducible analysis are archived in
**gpu-artifacts/evdi-copy-20261020/live-n7g38clw/**. The archive's
**export-profile.py**, **analyze-profile.py** and **analyze-live.py** reproduce
the exports and comparison; full results are in **results.json**.

### Rebuilding

The [preparation tool](../tools/gpu/prepare-evdi-copy.py) creates isolated
copies, applies the patch, runs the checks and builds the three modules.
Its output directory must not already exist:

```bash
python3 tools/gpu/prepare-evdi-copy.py --output /tmp/vino-evdi-copy-rebuild
```

LeakSanitizer needs a normal process environment; running under a tracing
sandbox can make its leak check fail before the test report is emitted.
Preparation never loads a module. The recorded artifact includes exact tool
copies, source trees, command arguments and binary hashes in addition to
the [investigation metadata](gpu-evdi-copy-investigation.json).
