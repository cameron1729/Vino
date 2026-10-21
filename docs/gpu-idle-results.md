## Idle capture and waiting

The completed live comparison exposed repeated full-screen captures of unchanged
pixels: its second Vino static sample captured about 60 images/second, checked
1,443 snapshots and sent no video bytes, while using 25.3% of one CPU core.
The first static sample had far fewer captures and used 3.3%. The compositor's
full-screen damage therefore cannot be treated as proof that pixels changed.
These observations identify work to avoid; they do not establish the cause of
the variable capture rate or the higher AMD power readings.

### Changes

The consumer now compares each completed RGB snapshot exactly against its
previous snapshot before invoking the strip planner. Equal images skip strip
hashing, cursor recomposition and frame logging unless a cursor change or the
dock's retransmit debt still requires presentation. The first black image also
retains its keyframe obligation. Comparison happens outside the mailbox mutex,
using owned buffers; capture can continue while the consumer checks pixels.

The capture thread now requests/grabs without waiting, then waits on DRM event
readiness when no frame is ready. Readiness wakes it immediately for frames,
cursor and control events. A 50 ms maximum wait bounds stop, queued DDC/CI
response and heartbeat handling. The consumer uses a condition variable and
the next eligible frame deadline, with maintenance bounded at 50 ms, instead
of repeatedly sleeping for two milliseconds. There is no capture-rate cap.
EVDI acknowledgement remains independent of encoding and USB.

Capture and RGB conversion still run for every captured update. The change
removes redundant downstream work and idle polling; it does not eliminate the
kernel's framebuffer copy or identify why X11 sometimes redraws an unchanged
output continuously. The codec, strip cache and dock delivery rules are unchanged.

### Offline measurements

Three alternating old/new pairs measured the actual mailbox delivery functions,
RGB conversion and existing strip planner with native release builds. Each
sample had a 30-frame warmup and at least one second of measurement. These are
component CPU timings, excluding EVDI ioctls, GPU encoding, USB and the compositor.

| Input | Before, CPU ms/image | After, CPU ms/image |
|---|---:|---:|
| Unchanged full image | 1.732 | 0.814 |
| Only the last visible pixel changes | 1.736 | 2.026 |
| Changing image | 1.767 | 1.788 |

The unchanged-image component uses about 53% less CPU. A change at the end of
the image pays for a full comparison as well as hashing, adding about 0.29 ms.
The short changing-image samples varied; they do not establish an active
throughput improvement. A separate two-second consumer-wait measurement counted
969 wakeups with the removed sleep loop and 40 with the new idle wait, using
0.01263 versus 0.00075 CPU seconds. That measurement has no device or producer.

No whole-driver CPU, power, temperature or fan improvement is established yet.
The [measurement JSON](gpu-idle-results.json) records the raw samples, scope,
checks and source/binary fingerprints. Baseline and final binaries, source
snapshots and the component harness are retained under
**gpu-artifacts/idle-capture-os4aq012/**.

### Diagnostics and validation

Health records now include cumulative unchanged-capture and event-wait counts,
RGB conversion/comparison time, presentation-loop iterations, and CPU counters
by thread. Optional AMD power, temperature, GPU clock and GPU busy readings are
sampled once per second. Missing sensors remain null; readings describe the
whole device and include other desktop activity. The comparison harness also
records optional GPU clock and busy readings for both backends.

Per-frame `capture_ms` now measures nonblocking grab plus conversion and related
bookkeeping; waiting on DRM readiness occurs separately. Do not compare it as
an identical stage to earlier logs, which included a wait inside capture.
Duplicate captures remain counted in health records even when no frame event
is emitted. USB presentation counts continue to mean submissions, not monitor FPS.

All 54 Chimera tests, 5 GPU tests and 42 Python live-trial/comparison tests pass.
Regression checks cover exact duplicate suppression, a last-pixel change, first
black output, retransmit debt, timed eligibility, idle maintenance, notifications
before/during waits, and wakeup for control events and failures. Strict GPU Clippy,
the release build and read-only hardware preflight pass. The subsequent live
trial completed and retained smooth dragging; its observations follow below.

### First live check

The two-minute desktop trial **/var/tmp/vino-live-twxtuai4** completed with a
successful worker exit and restoration, and the user confirmed “Yes, still
smooth” when asked about dragging Konsole and scrolling during the second
minute. Raw logs and the analysis script are archived under
**gpu-artifacts/idle-capture-os4aq012/live-twxtuai4/**.

| Observed period | Driver CPU, % of one core | GPU busy | AMD reported power |
|---|---:|---:|---:|
| First minute, mostly quiet desktop | 8.0% | 2.3% | 17.7 W |
| Quiet subwindow, elapsed 30.6–59.1 s | 5.9% | 1.1% | 17.7 W |
| Second minute, movement/scrolling requested | 35.5% | 17.7% | 19.1 W |

CPU uses cumulative process counters at the health-record boundaries. Sensor
means are time-weighted over those same windows; power belongs to the whole AMD
device. GPU clock readings were 800 MHz throughout. The quiet subwindow still
had about seven captures and 21 USB presentations per second: small regions
were changing, predominantly near the top of the display, and the dock required
retransmissions. It was not a completely static image.

In that quiet subwindow, the main thread used about 3.7% of one CPU core and the
capture thread about 2.1%. The last health record counted 1,199 identical captured
snapshots. Of these, roughly 1,020 still appeared in frame records because the
dock owed retransmissions; the duplicate counter is therefore not a count of
all hashing passes avoided. These required retransmissions currently re-enter
the strip planner, which still hashes the existing image. Avoiding that repeated
planning on a known-unchanged surface is a possible further CPU optimization.

This confirms the new code works on the live desktop and preserves the reported
smoothness. It does not establish a percentage reduction in live idle CPU or
power: the earlier 25.3%-CPU static-image sample had different capture activity,
and there was no matched old/new workload or power baseline in this trial.
Neither fan speed nor whole-laptop power was measured. The capture thread's
system CPU remains a substantial part of active load and is separately visible
in the JSON analysis.

### Live check

1. Run a two-minute trial, leave the Dell untouched for the first minute, then drag Konsole and scroll, and **verify** smooth movement and restoration of both original monitors:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 tools/gpu/live-trial.py run --encoder gpu --source desktop --fps 60 --duration 120
    ```
2. Inspect the resulting health records and **verify** that repeated identical captures increment `unchanged_captures`, USB bytes settle while idle, and new content resumes promptly

Vulkan validation is omitted for this CPU/power observation. The runner retains
the existing automatic recovery and bounded stop/watchdog behavior. A subsequent
controlled comparison needs freshly prepared inputs to match the new binary hash.
