## Comparison after disabling ZiRan cron

The run completed successfully: twelve measurements, no input-pacing or Vino
throughput warnings, no screen-lock failures, and both GPU workers exited
cleanly. Recovery succeeded. An independent desktop check confirmed the
original three-monitor layout, active official driver and compositor, EVDI
batching restored to zero, and no remaining trial units.

The result supports a substantial scrolling CPU improvement. It does **not**
demonstrate lower power, heat or fan noise. In this run, scrolling actually
measured higher AMD-reported power with Vino. That observation is retained,
even though unequal activity outside the measured display processes prevents
attributing the difference to the driver.

The experiment is closed for now with those conclusions. Another repetition
of the same configuration is not a prerequisite for accepting the CPU result.

### Measurements

Run `/var/tmp/vino-compare-xua6u47v` lasted **10 minutes 26 seconds**, from
**22:46:26 to 22:56:52 Australia/Perth on 20 October 2026**. It compared the
official driver with normal EVDI copying against Vino with 128-row EVDI
copying, using the same deterministic images at 20 submissions per second
on the Dell's 1080p/60 Hz mode. Each result below is the mean of two
thirty-second measurements in four-block ABBA order.

CPU percentages refer to one logical core. Power is the AMD PPT sensor
reading, not wall or whole-laptop power. These are all completed observations;
none passed the benchmark's separate power-qualification screen.

| Workload | Official driver CPU | Vino driver CPU | Official AMD power | Vino AMD power |
|---|---:|---:|---:|---:|
| Static image | 1.74% | 1.13% | 8.11 W | 7.44 W |
| Scrolling pattern | 43.41% | 30.00% | 10.37 W | 11.69 W |
| Moving colours | 23.37% | 25.57% | 9.42 W | 9.58 W |

Scrolling used **30.9% less driver CPU**, a reduction of 13.41 percentage
points of one core. Both repetitions agreed closely: official 43.38%/43.43%,
Vino 29.78%/30.22%. Including Xorg and KWin, CPU was **52.55% versus 37.28%**,
a reduction of 29.1%.

Moving colours used 9.4% more CPU in the Vino process, but total display-stack
CPU was nearly equal: **32.55% official versus 32.70% Vino**. Idle driver CPU
was lower with Vino by 0.61 percentage points of one core, and Vino sent no
video bytes in either static measurement. These are workload-dependent gains,
not a universal reduction.

The experiment compares two complete configurations, including the EVDI
optimisation. It does not isolate the benefit of GPU encoding alone.

### Power and remaining background activity

Disabling ZiRan cron removed that substantial source of interference.
The entire cron service used at most **0.021% of one core** in any measured
interval. Its largest CPU-time increment was 6.34 ms over thirty seconds.

The cgroup accounting now retains CPU from short-lived children. It measured
approximately **24–38% of a core** in the terminal autostart group and
**11–17%** in the Firefox group. These groups contain multiple processes;
the totals do not identify a particular tab, terminal widget or script.
CPU outside the display stack, player, sampler and lock helper ranged from
**0.39 to 0.63 cores**. The sampler itself used 3.1–3.4% of a core and was
accounted for separately.

All twelve samples exceeded the previously declared 0.25-core outside-CPU
limit, so all six power pairs remained excluded. Three pairs also exceeded
the 0.10-core mismatch limit. None exceeded the 5°C starting-temperature
spread limit. These diagnostic thresholds were unchanged; exclusion does not
mean execution failed or that the raw readings contain no information.

Scrolling power was higher with Vino in both repetitions:

| Repetition | Official AMD power | Vino AMD power | Official outside CPU | Vino outside CPU |
|---|---:|---:|---:|---:|
| First | 10.39 W | 11.89 W | 0.427 cores | 0.627 cores |
| Second | 10.34 W | 11.49 W | 0.392 cores | 0.609 cores |

The terminal group accounted for roughly 0.24 cores during official scrolling
and 0.38 during Vino scrolling. GPU busy readings also increased, from a mean
of 5.2% to 19.1%. The observations cannot distinguish unrelated background
variation from work influenced by the display condition, or isolate the GPU's
energy contribution. No CPU amount has been converted into or subtracted
from watts. Vino may cost more energy for this workload; lower driver CPU
alone is not evidence of lower energy.

Total system CPU during scrolling was 0.99 cores with the official driver and
1.05 with Vino. The lower display-stack CPU therefore did not produce a lower
whole-system CPU reading in this run.

Idle power differences changed direction between repetitions, as did the
moving-colour differences. Mean ThinkPad fan readings stayed around 1,960 RPM
in both configurations. There is no demonstrated cooling or acoustic benefit.
The separate ACPI fan reading is retained in the raw data and is not averaged
with the ThinkPad tachometer.

### Evidence and limits

The 59 raw measurement, event, progress, state and recovery files are archived
with verified checksums in
`gpu-artifacts/comparison-monitor-20261020/run-xua6u47v/`.
Its `analyze.py` independently recomputes CPU, cgroup, power and thermal
arithmetic, verifies prepared source/binary identities, and checks screen-lock,
GPU events and desktop recovery. All checks passed.

The [analysis JSON](gpu-live-cron-off-results.json) includes every observation
and the unchanged power exclusions. The preceding
[monitored run](gpu-live-power-comparison-results.md) remains available
separately; its numbers have not been pooled with this run.

The user reported this run finished. No additional visual assessment was
requested; the preceding run was reported smooth with both drivers. Input
timing and changed-update counts do not prove equivalent codec quality,
physical presentation rate or sustained 60-FPS output. Production readiness
and sustained thermal behaviour are not established by this benchmark.
