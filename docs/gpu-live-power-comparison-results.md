## Power comparison with background and screen-lock monitoring

Run `/var/tmp/vino-compare-gkfla9l7` completed all twelve measurements on
20 October 2026, **22:17:28–22:27:54 Australia/Perth**, taking 10 minutes
26 seconds. Both Vino workers exited cleanly, recovery reported success, and
an independent check confirmed the official service, compositor and all three
original monitor positions. EVDI batching returned to zero.

The user confirmed that **both drivers looked similarly smooth on the Dell**.
There were no input-pacing or Vino changed-update warnings. All 626 screen-lock
guard events were readiness or successful heartbeats, with no lock failures.
Each measurement had a successful lock-state check after its end.

The improved instrumentation worked, but **none of the six power pairs passed
the declared background-load checks**. This run does not establish a power,
heat or fan advantage.

### Observed CPU

The comparison used official/normal EVDI copying versus Vino/128-row copying,
with identical synthetic input at 20 images per second on the Dell's 1080p/60 Hz
mode. It includes both Vino and the EVDI optimisation. Each table entry is the
mean of two thirty-second measurements, expressed as a percentage of one
logical CPU core.

| Workload | Official driver | Vino with batching | Vino difference |
|---|---:|---:|---:|
| Static image | 4.36% | 1.03% | Lower, but official repetitions varied substantially |
| Scrolling pattern | 37.80% | 30.30% | 19.8% less driver CPU |
| Moving colours | 21.26% | 23.97% | 12.7% more driver CPU |

Including Xorg and KWin, scrolling was 45.97% versus 37.28%, while moving
colours was 30.02% versus 30.63%. The moving-colour difference is therefore
small for the complete display stack. Official idle repetitions were 7.18%
and 1.53%; Vino's were 0.87% and 1.20%. Vino sent no USB video bytes in either
measured static interval.

These are observed CPU results under variable background activity, not a
controlled causal estimate. They continue to support a workload-dependent CPU
benefit rather than a universal improvement.

### Why the power comparison is still inconclusive

CPU outside the display stack, replay player, sampler and lock helper ranged
from **0.40 to 1.84 cores** across samples. Every sample exceeded the declared
0.25-core limit, and every matched pair exceeded the 0.10-core spread limit.
Three pairs also exceeded the 5°C starting-temperature spread limit.
The thresholds have not been changed after seeing these results.

The two Vino scrolling samples used nearly the same driver CPU, **29.63% and
30.97%**, but AMD-reported power was **9.72 W and 20.93 W**. Background load was
0.44 and 1.26 cores, with starting AMD temperatures of 48°C and 62°C.
Comparing average watts across drivers would conceal this large variation.

The process snapshots and scheduler logs identify actual overlapping work:

- ZiRan's `picker.update.cache` ran from **22:18:06 to 22:19:46**, a
  hundred-second run during the first official-driver block
- `moodle.update.upstreams` ran from **22:22:06 to 22:23:34**; other repository,
  calendar and Baiyan jobs also ran during the comparison
- Cron launched the hourly task group at 22:17, and `fusioninventory` processes
  appeared in early measurements; the configured hourly task is
  `fusioninventoryagent`
- Snapshots recorded hundreds of distinct short-lived processes, including
  Bash, Git, SSH, awk and sort processes under the cron service

These establish overlapping background activity, not a complete allocation of
the excess CPU to particular jobs. One-second process scans only attributed
about 0.24–0.34 background cores per sample; considerable short-lived work and
other system CPU remained unattributed. Service/cgroup counter deltas would
retain CPU from exited processes and should supplement these snapshots.

The sampler itself used approximately 2.8–3.4% of one core, recorded separately
and applied to both drivers. It does not account for the much larger residual.
CPU temperature, individual fan readings and GPU activity were retained and
independently recomputed, but their observed differences cannot be attributed
to the driver under these conditions. AMD PPT is not wall or whole-laptop
power, and short fan samples do not establish steady-state noise.

### Next experiment

Temporarily defer ZiRan's scheduled maintenance during a bounded comparison
and restore its schedule afterward; also wait for the hourly inventory task
to finish. Keep the same source workload, driver binaries and declared
comparison checks. Record service/cgroup CPU deltas alongside process snapshots
so short-lived work remains attributable if background load persists.

No schedules, jobs or drivers were changed while analysing this run.
No repeated display takeover was started. This avoids spending another trial
with the same known sources of interference.

All CPU counters, power integrals, thermal/activity means, Vino event counts,
guard records, source hashes and recovery state were checked. The 58 archived
raw files retain their original checksums under
`gpu-artifacts/comparison-monitor-20261020/run-gkfla9l7/`, with scheduler
evidence and a reproducible `analyze.py`. The
[analysis JSON](gpu-live-power-comparison-results.json) includes all observations
and exclusions. Visual smoothness is confirmed; equal codec quality,
physical presentation rate and sustained 60-FPS output remain unmeasured.
