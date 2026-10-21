## Live official-driver comparison

`tools/gpu/compare-live.py` compares the installed DisplayLink service with the
complete Vino GPU desktop path, including EVDI capture and USB transport. This
is separate from the older live-official/offline-Vino experiment in
[`gpu-official-comparison.md`](gpu-official-comparison.md). Its results must not
be combined with that experiment's two-monitor numbers.

The first full physical comparison completed on 18 October 2026, and the user
reported smooth output. The [results](gpu-live-comparison-results.md) show lower
observed Vino CPU usage, mixed power readings and a throughput qualification;
equal-output efficiency and lower heat are not yet established.

The subsequent [three-condition result](gpu-live-comparison-evdi-results.md)
compares official, Vino/normal-copy and Vino/batched-copy at twenty input
images per second. Execution and recovery succeeded, but the last official
idle sample is excluded because screen-lock greeter startup overlapped it.
All active samples preceded that startup. Use the qualified analysis rather
than the raw harness's idle summary; background activity still limits power
conclusions.

The updated harness records background processes, CPU temperatures and each
available fan separately, and inhibits automatic screen locking for the
comparison's lifetime. It keeps all raw observations but separately identifies
matched repetitions that pass declared background-load and temperature checks.
The [subsequent physical run](gpu-live-power-comparison-results.md) completed
without lock or pacing problems, and the user confirmed both drivers looked
smooth. All six power pairs were excluded for background load and mismatch;
the new logs identified overlapping scheduled maintenance.

The [comparison after disabling ZiRan cron](gpu-live-cron-off-results.md)
also completed and recovered cleanly. Scrolling used about 31% less Vino
driver CPU, while total display CPU for moving colours was nearly equal.
Scrolling measured higher AMD power with Vino; terminal and browser activity
still varied, and no power or heat advantage was established. The CPU result
is retained without requiring another repetition of this configuration.

The [latest direct-capture comparison](gpu-live-direct-comparison-results.md)
completed with reported good visual behaviour. Current Vino used 10.5% less
scrolling driver CPU and 18.9% less moving-colour driver CPU than proprietary.
Scrolling nevertheless used more CPU than the older Vino build; thread counters
locate that increase in main-thread kernel work. The results retain that
regression alongside the wins.

### Current combined configuration

The next head-to-head selects `--capture direct --gpu-evdi-rows 128` for Vino.
It includes the fused GPU encoder, direct capture into mapped GPU memory and
128-row EVDI copying. The official service uses its normal per-row EVDI path.
The same static, scrolling and moving-colour inputs run for both drivers in
the existing four-block ABBA order at twenty image submissions per second.

Direct capture selects client cursor events automatically. Both the read-only
worker check and the actual staged worker receive that selection, and the
comparison records the selected capture and cursor modes. Direct capture requires
head zero. The default copied path omits the new worker flags so archived workers
remain usable.

### Separating Vino from the shared EVDI improvement

Both the installed Linux DisplayLink service and Vino use EVDI to obtain
desktop frames. The experimental EVDI copy patch is separate from Vino's
Vulkan encoder and could also benefit the official service. A comparison of
only official/normal-copy against Vino/batched-copy would combine those effects.

The prepared comparison uses `--compare-evdi-rows 128` to measure three
conditions on the same temporary EVDI candidate:

| Condition | Display driver | EVDI rows per copy |
|---|---|---|
| official-evdi-0 | Installed proprietary DisplayLink service | Original per-row path |
| gpu-evdi-0 | Current Vino GPU implementation | Original per-row path |
| gpu-evdi-128 | Same Vino worker | Up to 128 contiguous rows |

The first pair measures official → Vino/normal → Vino/batched; the second
reverses that order and rotates the workload order identically for each
condition. This produces eighteen samples in six blocks. Default warmup and
measurement durations take approximately 16–18 minutes including transitions;
the measurement supervisor's limit is 1,480 seconds, followed by bounded
recovery. The two-condition default remains available without the EVDI option.

The official and Vino/normal conditions compare complete implementations on
the same capture path; they do not isolate the compression kernel alone.
Vino/normal against Vino/batched measures the additional EVDI benefit.
Official/batched is not included in this first comparison, so its possible
benefit remains unmeasured. Zero uses the original copy loop in the candidate
module; the installed original module is not reloaded between blocks.

For this run, `--input-fps 20` supplies twenty synthetic images per second,
below Vino's observed submission rate in the earlier heavy scrolling workload.
The display stays at 60 Hz, the Vino presentation cap stays at 60 FPS, and
independent capture acknowledgements remain unchanged. This rate is intended
to reduce saturation; it does not guarantee equal delivered output and is not
a test of 60-FPS full-screen motion.

Each measurement checks the loaded EVDI source version and batch setting
before and after sampling. The official service always runs with batching
disabled. The coordinator records reset intent and resets batching before
starting that service; independent recovery resets it again after completion,
failure or interruption. A changed module or batch value invalidates the
measurement. The current candidate must still be loaded when starting this
comparison; the preflight refuses a completed reboot into the original module.

Vino results distinguish changed updates from required retransmissions.
Fewer changed updates than 95% of replay submissions produces a visible
throughput warning; extra retransmissions cannot conceal that shortfall.
This is a diagnostic count with a boundary/scheduling allowance, not source
frame identification or proof of physical monitor FPS. A run without warnings
still needs visual assessment and retains the output-equivalence limitations.
CPU savings remain distinct from AMD-reported power savings and fan behavior.

For the current comparison, `--gpu-evdi-rows 128` selects just
official/normal-copy and Vino/batched-copy in the usual four-block ABBA order.
Use it with `--capture direct --input-fps 20` to compare the two complete setups in approximately
11–13 minutes. This includes both the Vino encoder and EVDI optimisation; it
does not attribute a difference exclusively to GPU encoding. The two EVDI
options are mutually exclusive, and both require the prepared candidate to be
loaded with batching initially disabled.

### Method

Both backends drive the same Dell, identified by its complete EDID, at
1920×1080/60 Hz. The laptop is primary at 0×0, the Dell sits immediately to its
right and the BenQ is disabled. KWin's existing compositor state is preserved
and checked before and after each sample. The replay window requests that the
compositor remain in the path. The GPU encoder runs without Vulkan validation.

The default four blocks run official → GPU → GPU → official. Every block has
15 seconds to settle, followed by static, scrolling and moving-image samples,
each with 15 seconds of warmup and 30 seconds of measurement. The second pair
rotates the case order identically for both drivers. Every GPU takeover starts
from a working official-driver state, including the two adjacent GPU blocks.
The normal run takes about 11 minutes; the supervisor limits measurement work
to 990 seconds, followed by bounded recovery.

The player reuses head zero of the deterministic 16-frame BGRX corpus exported
by `vino-gpu`. Active cases submit the same full-window images at 60 inputs per
second. Static holds submit one image before warmup and no images during
measurement. The 16-frame loop has an intentional discontinuity when it wraps;
judge differences between backends using the same loop. This is a synthetic
input and damage workload, not actual video decoding or window dragging.

A native X11 MIT-SHM player checks 16 pixels from its own window before each
sample. It then gates the measured interval through a pipe, remaining alive
with the last image while the supervisor reads final CPU counters. No desktop
screenshots or USB payload captures are saved. Source files, binaries and
harness scripts have their SHA-256 identities recorded.

### Measurements and interpretation

- Official driver CPU comes from the service's cgroup; Vino CPU comes from its
  process, including all its threads and kernel work charged to them
- Xorg, KWin, player and sampler process CPU are recorded separately, with PID
  start-time checks to reject process replacement or restarted counters
- Display-stack CPU is driver + Xorg + KWin, excluding the shared input player
  and measurement coordinator
- Total system busy CPU provides background-load context and includes work
  outside those processes; the residual excludes the display stack, player,
  sampler and screen-lock helper
- Approximately once per second, `/proc` process CPU counters, names, start
  times and cgroups are recorded without command arguments; new processes and
  PID reuse are distinguished, and official-service children are included in
  its driver category
- AMD hwmon power and temperature are sampled roughly five times per second;
  power is integrated using the actual sample intervals and reported as the
  sensor's PPT reading, not wall or whole-laptop power
- GPU busy percentage and clock readings, available CPU temperatures, and
  each readable fan tachometer are retained with time-weighted summaries and
  coverage fractions; missing readings remain missing, not zero
- Raw sensor samples, timing intervals, X11 input rate/deadline misses and Vino
  USB presentation counts are retained

CPU values use units of one logical processor's worth of CPU time: `0.38`
means 38% of one CPU core. CPU reduction alone does not prove lower power.
Compare the repeated power readings and temperatures as well as display-stack
CPU, and check whether input pacing, background load and visual results are
comparable. Warmup and ABBA ordering reduce startup/order effects but do not
establish a fully controlled thermal steady state.

Input submissions, XSync and Vino USB presentations do not measure physical
monitor FPS or latency. Official codec/quality adaptation is not forced to
match Vino. The report leaves monitor FPS and output-quality equivalence
unverified; a visual comparison is required before interpreting lower resource
usage as a useful improvement. The report includes each repetition and its
mean/min/max, not confidence intervals.

Timing variation is recorded as a result for both drivers. More than 1% of
replay deadlines missed, a replay gap of at least 100 ms, or duration drift of
at least 250 ms produces a visible warning and marks that sample's input pacing
as unqualified. The run continues so the other driver can be measured. The
summary includes pacing warnings, actual input rate, deadline-miss percentage,
submission p99 and maximum gap alongside CPU and power. A completed report with
warnings does not establish savings at equivalent input pacing.

Samples still fail if processes or the display environment change, the player
exits early or submits an incomplete/wrong workload, CPU counters reset,
sampling intervals drift by 100 ms, or power sampling has a one-second gap.
Failures identify the case and measured reason. Vino startup and its ongoing
capture/USB watchdog are also checked. Partial runs remain marked incomplete
and do not produce a completed-comparison summary. Schema 3 distinguishes
completion from passing input-pacing checks, keeps each EVDI condition separate,
and records the input rate and Vino changed-update throughput.

Schema 4 adds process attribution, thermal/activity summaries, per-sample
screen-lock evidence and a separate `power_comparison` assessment. The ordinary
`summary` contains all completed observations, including noisy ones. Completion
alone does not make those observations a dependable power comparison.

### Power comparison checks

The harness reports the largest CPU consumers outside the tracked processes.
It also retains `*-processes.json` beside the timestamped `*-power.json`, so
power excursions can be compared with process activity. Process scans cannot
observe processes born and exited between scans, or their final CPU after the
last sighting. Deferred kernel work may itself relate to the display. Treat
attribution as an explanation aid, not an exact partition of system CPU or
power; no background CPU figure is converted to or subtracted from watts.
The counters at each measurement boundary also retain cgroup CPU usage and
produce a `cgroup_attribution` diagnostic. Service counters retain CPU from
short-lived children that process scans miss. These are inclusive hierarchy
counters: parent and child rows overlap and must never be summed. Groups
created, deleted or replaced during the interval have no direct delta, though
surviving ancestors retain their charged CPU. A group may contain both display
and background work, so this diagnostic does not change the power checks.
The [kernel documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html#cpu-interface-files)
describes the inclusive `cpu.stat` accounting.
The sampler and guard's own CPU usage is recorded separately. Instrumentation
also costs power and is applied to both conditions; a read-only check on this
machine took roughly 20 ms of CPU per process scan, plus about 1 ms per sensor
sample. These measurements include that instrumentation, not a hypothetical
zero-overhead observer.

The default checks are set before the run:

| Check | Default |
|---|---|
| CPU outside tracked processes, per sample | At most 0.25 of one core |
| Outside-CPU spread across conditions in one repetition | At most 0.10 of one core |
| Starting AMD sensor temperature spread across those conditions | At most 5°C |
| Matched repetitions needed for repeated power observations | At least two |

`--max-background-cpu`, `--max-background-delta` and
`--max-temperature-delta` expose the first three thresholds, and the report
records their values. These are pragmatic diagnostic checks, not calibrated
error bounds. Choose them before collecting data, not afterward to favour a
driver.

A matched repetition includes every requested condition for the same workload
and repeat number. If any condition fails the checks, the **whole repetition**
is excluded from `power_comparison` for that workload. Input pacing, completed
sampling, screen-lock monitoring and Vino changed-update checks must also pass.
The raw observations remain available and the run continues after background
warnings. Missing repetitions produce an explicit inconclusive result rather
than comparing one driver's quiet samples against another driver's busy ones.

Even passing these checks does not prove equal monitor output, thermal steady
state or isolated driver power. Temperatures and fan speeds lag workload changes;
short samples provide context rather than a sustained fan-noise result. Individual
fan readings are never averaged together: on this machine ACPI and ThinkPad
interfaces report different values.

### Screen locking and interruption

Before any display takeover, a helper running as the desktop user acquires
KDE's `org.freedesktop.ScreenSaver.Inhibit` on a persistent D-Bus connection.
It listens for `AboutToLock`, `ActiveChanged` and service replacement, and checks
the lock state once per second. Each measured interval requires a fresh
successful check after it ends. Even a brief lock/unlock is latched as a failure.
The preflight refuses an already locked desktop.

This changes no saved screen-lock settings and does not simulate input or unlock
the session. Manual locking still works; it stops the comparison and triggers
normal display recovery. A dead or unresponsive helper also stops the test.
Its stdin lifetime and the existing systemd cgroup bound it to the coordinator;
normal shutdown explicitly releases the cookie, and KDE releases it if the
connection disappears, including after a crash. This follows the installed
[KDE 5.27 interface implementation](https://github.com/KDE/kscreenlocker/blob/Plasma/5.27/interface.cpp).

`screen-lock.jsonl` records readiness, checked timestamps and failures.
An interrupted replay saves `*-interrupted.json` and its partial power/process
samples without qualifying that interval. Previously completed measurements
remain in the incomplete report.

External-power checks read laptop Mains inputs and system-scoped USB/wireless
inputs. Internal batteries, mouse/keyboard batteries and device-scoped chargers
are excluded. A genuine input change reports the supply name and its exact
before/after online values. Completed replays also save provisional raw CPU,
power and input measurements before the final environment check, so a failed
check leaves diagnostic data without qualifying that sample for the comparison.

The initial physical attempt (`/var/tmp/vino-compare-q7x1wcbn`) completed the
official idle and scrolling samples before the original strict pacing gate
aborted it. Scrolling submitted all 1,800 images in 30.000065 seconds, with
117 late submissions (6.5%) and a maximum gap of 43.10 ms; CPU and power sampling
passed. This prompted the warning policy above. Recovery succeeded, but no
Vino sample was reached, so that attempt provides no driver comparison.

The second attempt (`/var/tmp/vino-compare-wve8nzzj`) completed all three official
samples and Vino's idle/scroll samples, then aborted after Vino's motion replay
with the old generic power-source error. The old check included two Logitech
mouse batteries in its snapshot and did not save the mismatching reading.
The exact changed entry therefore cannot be reconstructed, and this error does
not establish a charger change. Recovery succeeded. Peripheral power entries
are now excluded, with regression checks for mouse sleep, wake and removal.

### Running

Preparation and read-only checks passed for the current local build and loaded
temporary EVDI candidate. After rebuilding Vino or changing the replay source,
regenerate the inputs and run read-only checks:

```bash
python3 tools/gpu/compare-live.py prepare
python3 tools/gpu/compare-live.py preflight --capture direct --input-fps 20 --gpu-evdi-rows 128
```

1. Unlock the desktop, pause videos, builds and other active workloads, keep the laptop connected to the same power source and power profile, and leave the pointer on the laptop display
2. Run the comparison from your normal desktop terminal:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 /home/cameronball/Projects/Vino/tools/gpu/compare-live.py run --capture direct --input-fps 20 --gpu-evdi-rows 128
    ```
3. **Verify** the static pattern, scrolling pattern and moving colours appear on the Dell for each backend, and note any visible difference in smoothness or quality while leaving the desktop otherwise idle
4. Leave the machine otherwise unused until the run finishes, or press Ctrl+C to restore early, then **verify** both dock displays and their original positions return with `evdi_copy_rows_restored: 0` and no recovery errors
5. Return the printed log directory and visual observations for analysis

The terminal prints every backend/case result and timing warning, including
updates that arrive between its polling intervals. Logs and `measurements.json` are
written under a unique `/var/tmp/vino-compare-*` directory. `recovery.json`
records whether the official service and original display layout were restored.
Escape also aborts an active replay. No runtime drivers, kernels, firmware or
system libraries are installed by this tool.

### Recovery and validation

One systemd service owns the entire comparison, its player processes and Vino
workers. The original layout is saved before any changes. A separate bounded
recovery service restores the official driver and matches saved monitors by
EDID after normal completion, a failed startup, a crash, a watchdog failure,
runtime expiry or interruption. The recovery hook is armed before the first
official-driver layout change, not only before Vino takeover. Dock audio and
network bindings are checked against their original values. The comparison
shares the live trial's lock and unit exclusion, preventing simultaneous trials.

Validation covers the real C player on private Xvfb, the complete telemetry
collection path, CPU attribution with PID reuse and exited processes, missing
sensor readings, matched-pair exclusions, and preservation of interrupted
measurements. Private D-Bus tests cover scoped inhibition, manual and brief
locks, lock acquisition, service replacement, helper termination, loss of the
parent pipe, stale heartbeats, and cleanup after coordinator failure. No test
locks the real desktop or changes its outputs.

All seven systemd supervision/recovery scenarios also pass with simulated
devices. Earlier EVDI/reset, timing, throughput and recovery regressions remain
covered. Current validation evidence is retained under
**gpu-artifacts/comparison-monitor-20261020/**; the older three-condition
preparation and physical-run evidence remains under
**gpu-artifacts/comparison-evdi-20261020/**.

```bash
python3 -m unittest discover -s tools/gpu -p 'test_live*.py' -v
python3 tools/gpu/compare-live.py self-test
```
