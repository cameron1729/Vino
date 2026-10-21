## Direct capture versus the official driver

The comparison **/var/tmp/vino-compare-pygb7ttl** completed all twelve
measurements on 21 October 2026, from **01:20:45 to 01:31:12 Australia/Perth**.
The user reported that it looked good. Both Vino workers exited cleanly,
recovery succeeded, and an independent check confirmed the original three
monitor positions, active official driver and compositor, and batching reset
to zero.

The current Vino build used fused GPU encoding, direct capture, client cursor
events and 128-row EVDI copying. The proprietary driver used normal EVDI
copying. Both received identical synthetic images at twenty submissions per
second on the same Dell in 1080p/60 Hz mode, in four-block ABBA order.

### CPU results

These are means of two thirty-second measurements, expressed as percentages
of one logical CPU core:

| Workload | Proprietary driver | Current Vino | Less driver CPU |
|---|---:|---:|---:|
| Idle | 1.50% | 0.73% | 51.2% |
| Scrolling pattern | 42.17% | 37.75% | 10.5% |
| Moving colours | 23.36% | 18.94% | 18.9% |

Both repetitions favoured Vino in every workload. Including Xorg and KWin,
scrolling used **51.33% versus 45.81%** of one core, and moving colours used
**32.77% versus 25.72%**. These correspond to 10.8% and 21.5% lower display-stack
CPU. Idle's relative reduction is large but its absolute driver saving is
only 0.77 percentage points of one core.

All twelve intervals passed input pacing and sampling checks. Vino produced
599 changed updates for each 600-image scrolling measurement and 600 for
each moving-colour measurement, with no throughput warnings. These counts
do not measure physical monitor FPS or prove identical codec quality.

Both Vino workers reported direct capture, with zero desktop pixel uploads,
zero RGB snapshots and no pool exhaustion. The idle measurements sent no
video bytes. No cursor fallback or Vulkan errors were reported.

### Power observations

| Workload | Proprietary AMD power | Vino AMD power |
|---|---:|---:|
| Idle | 7.41 W | 7.30 W |
| Scrolling pattern | 10.91 W | 11.02 W |
| Moving colours | 9.10 W | 8.32 W |

Moving-colour power was lower with Vino in both repetitions. Idle and
scrolling differences changed direction between repetitions. These are
whole AMD-device readings, including other work, and do not establish a
general heat or fan improvement.

All pairs exceeded the harness's existing absolute background-CPU threshold,
while all passed its background-spread and starting-temperature-spread
thresholds. The raw readings and CPU results are retained. The result does
not depend on subtracting estimated background watts or requiring another
run with a more isolated desktop.

### Scrolling regression to investigate

The older [cron-off comparison](gpu-live-cron-off-results.md) measured
approximately 30% Vino driver CPU for the same scrolling input, compared
with 37.75% now. Moving-colour CPU improved from 25.57% to 18.94%.
The latest build therefore does not improve every workload over that older
Vino build, despite beating the proprietary driver in this run.

Existing thread counters locate the scrolling increase in kernel CPU charged
to the main presentation thread. Capture-thread CPU and main-thread userspace
CPU both decreased; main-thread kernel CPU increased from about 4.3% to
17–19% of one core. Encoded strip counts and USB byte volumes are comparable,
and there were no cursor events or repair copies in those intervals.
This identifies the next investigation target; the counters alone do not
identify the responsible syscall or kernel routine.

The final USB packet assembly was a concrete code lead: in that build,
`prefix_frame` built a complete multi-megabyte stream and copied it into fresh 64-KiB chunks
for every presentation. At the measured scrolling traffic of roughly 110 MB/s,
those two copies write roughly 220 MB/s before the kernel USB copy. This work
falls inside the reported USB stage and outside the offline codec/framing
replay. Its allocation pattern is a candidate explanation, not a demonstrated
cause of the increased kernel CPU.

The [USB reuse follow-up](gpu-usb-reuse.md) now removes the intermediate stream
copy and retains packet buffers and libusb transfer objects. Its offline
validation and component results are recorded separately; these comparison
numbers describe the earlier build.

### Evidence

Raw measurements, worker events, power samples, recovery records and source
identities are archived under
**gpu-artifacts/comparison-direct-20261021/run-pygb7ttl/**.
The independent analysis recomputes process/cgroup CPU differences and power
integration for all twelve intervals, verifies the selected modes and accessible
input/build hashes, and records its limitations. The protected staged worker
and helper files could not be independently reread without root; the runner
validated the staged worker hash before launch.

The reproducible calculation is **analyze-independent.py** and its output is
**independent-results.json** in that directory. The archive manifest records
the 59 copied raw files and the two protected per-block state files left in
the original run directory.
