## First completed live comparison

The full official/Vino comparison completed on 18 October 2026, and the user
reported “k finished and smooth”. All twelve samples completed, both Vino
workers exited cleanly and the original display layout was restored. Vino used
less driver CPU in the active samples, but this run does not establish better
efficiency at equal output or lower heat. Power readings were mixed, and Vino
submitted fewer USB updates than the 60 images/second offered by the player.

The source run is `/var/tmp/vino-compare-pi080821`. Readable evidence is archived
under `gpu-artifacts/live-comparison/run-pi080821`. The [analysis JSON](gpu-live-comparison-results.json)
contains the original measurements, recovery result, per-repetition analysis
and source hashes. The [method](gpu-live-comparison.md) describes the player,
supervision and measurement boundaries. Earlier aborted attempts are excluded.

### Observed CPU and power

Each number below is the mean of two 30-second samples after 15 seconds of
warmup. CPU is a percentage of one logical processor's time. Both drivers
actually drove the same Dell at 1920×1080/60 Hz, with the laptop primary and
BenQ disabled; Vino included capture and USB transport and ran without Vulkan
validation. The installed baseline was DisplayLink 6.4.0-22 with EVDI 1.15.1-197.

| Workload | Official driver CPU | Vino driver CPU | Observed CPU reduction | Official AMD PPT | Vino AMD PPT |
|---|---:|---:|---:|---:|---:|
| Static image | 18.3% | 14.3% | 21.8% | 14.5 W | 19.5 W |
| Scrolling pattern | 85.4% | 44.3% | 48.1% | 23.6 W | 22.5 W |
| Moving colours | 52.8% | 41.4% | 21.6% | 20.5 W | 21.7 W |

Including Xorg and KWin, scrolling CPU fell from 106.2% to 64.8% of one core;
motion fell from 72.2% to 61.3%. This is evidence of lower observed CPU usage
for these runs, not an equal-output encoder speedup. The shared input player
and sampler are measured separately and excluded from these display-stack sums.

Static CPU varied substantially between repetitions: official 7.7%/28.9%, Vino
3.3%/25.3%. System-wide background load also varied. During the second Vino
static sample, CPU outside the measured display stack, player and sampler was
about 1.93 cores, versus about 1.00 core in the second official static sample.
The two static means should not be treated as precise idle baselines.

Power was about 1.2 W lower with Vino during scrolling, 1.2 W higher during
motion and 5.0 W higher during static input. These are AMD-reported PPT sensor
readings, not whole-laptop or wall power. Power varied across repetitions;
background activity, automatic clocks and thermal history remained uncontrolled.
The readings therefore do not support a cooler/quieter-laptop claim, nor do
they isolate Vino as the cause of the higher static readings. Fan speed was not
measured. Mean sensor temperatures were also slightly higher for Vino in each
case, with the same thermal and background-load limitations.

### Input cadence and Vino throughput

Every active sample submitted all 1,800 input images in approximately 30 seconds.
The two official scrolling samples had 116 and 675 late X11 submissions; both
were retained with timing warnings. Vino scrolling had zero late submissions.
The player supplied the same source image sequence and average rate, but the
timing distribution was not identical. The user reported smooth overall output;
physical monitor FPS, latency and pixel-quality equivalence were not measured.

Vino's own logs show the following rates within the measured windows:

| Workload | Input images/s | Vino USB updates/s, repeat 1 | Vino USB updates/s, repeat 2 | Vino USB traffic |
|---|---:|---:|---:|---:|
| Static image | 0 | 0 | 0 | 0 MB/s |
| Scrolling pattern | 60 | 29.6 | 28.8 | 94.5–95.4 MB/s |
| Moving colours | 60 | 45.9 | 47.3 | 53.0–54.0 MB/s |

These are USB submission counts, not measured monitor FPS. The official
driver's corresponding output count is unavailable. The lower observed Vino
CPU cannot therefore be described as a 48%/22% saving at equal delivered FPS.
This workload changes nearly the entire display on each input; it does not
establish the same throughput limit for ordinary desktop damage or a smaller
video window. Vino intentionally retains the newest captured image while it
finishes the current encode/USB transfer.

The recorded Vino stage times explain the observed rate under this workload:

| Workload | Mean encode time per submitted update | Mean USB time per submitted update |
|---|---:|---:|
| Scrolling pattern | 13.5–13.6 ms | 20.1–20.8 ms |
| Moving colours | 10.0–10.6 ms | 11.0–11.1 ms |

The current path serializes these stages. Their combined time is approximately
34 ms for scrolling and 21–22 ms for motion, consistent with the submission
rates above. Capture runs independently, so its elapsed time must not be added
to those stage times. USB timing includes the current implementation's waits;
these measurements do not establish the USB bus's maximum bandwidth.

### Implications

The live prototype passed this functional comparison and used less measured
CPU while the user found it smooth. The remaining questions are equal-output
efficiency and power: a lower input cadence that both paths can sustain would
reduce the throughput confound, while output counting would make the official
comparison stronger. Vino's encode/USB serialization and high scrolling payload
rate are concrete throughput targets. Investigating static GPU clocks and
background activity would help explain power without assuming that CPU savings
must produce less heat. No further optimization or display trial was started
as part of this analysis.

### Evidence checks

The analysis checked all twelve complete frame counts, CPU/power sampling flags,
unchanged system-power inputs at the environment checks, both successful GPU
exits and successful recovery. Power means were recomputed from all raw sensor
samples; USB counts and bytes were independently recomputed from the frame
events. Both matched the recorded report. The two input-timing warnings remain
visible in the source data and interpretation.
