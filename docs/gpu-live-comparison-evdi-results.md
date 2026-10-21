## Official, Vino and EVDI batching comparison

The three-condition comparison **/var/tmp/vino-compare-toc_di9u** completed all
eighteen samples on 20 October 2026, from 19:56:59 to 20:12:29 Australia/Perth.
All four Vino workers exited successfully. Recovery reported no errors and
reset EVDI batching to zero; an independent desktop check confirmed the official
service, compositor and all three saved display positions.

The user found the laptop locked afterward. The screen-lock greeter's first
recorded startup message is at **20:12:01.018697**, monotonic
**1588243.291505 seconds**, within the final official-driver idle measurement
at **1588241.512114–1588271.512176**. That sample is excluded from this analysis.
The other seventeen measurements, including all twelve active samples,
precede the observed greeter startup. This timestamp is a startup log, not
an instrumented lock-state transition.

The raw harness report says `complete: true` and includes the affected idle
sample in its summary. Completion describes execution and recovery, not
measurement validity. Use the qualified analysis here instead of that raw
idle mean. The harness did not inhibit or monitor screen locking; future runs
need explicit lock handling before treating unattended completion as valid.

### CPU results

The same Dell ran at 1920×1080/60 Hz with twenty synthetic input images per
second during active samples. Each condition ran twice, with reversed condition
order and matched case order. All conditions used the same temporary EVDI
candidate: zero selects the original per-row copy path; 128 enables batching.
Only the Vino batched condition enabled batching.

The values below are means of two thirty-second active measurements, expressed
as a percentage of one logical CPU core.

| Workload | Official, normal copy | Vino, normal copy | Vino, 128-row copy |
|---|---:|---:|---:|
| Scrolling pattern | 40.84% | 32.30% | 29.52% |
| Moving colours | 24.79% | 26.30% | 24.68% |

For scrolling, Vino used 20.9% less driver CPU with the same copy path.
Batching reduced Vino's driver CPU by a further 8.6%, producing an observed
27.7% reduction relative to the official baseline. For moving colours, Vino
with normal copying used about 6.1% more CPU than official; batching reduced
Vino's CPU by 6.1%, bringing it approximately level with official. The result
supports a workload-dependent CPU improvement, not a universal speedup.

Including Xorg and KWin, mean display-stack CPU was 49.56%, 38.65% and 36.62%
for scrolling, respectively, and 33.89%, 33.05% and 31.53% for moving colours.
The shared player and sampler are excluded from those totals.

After excluding the lock-affected sample, the official idle result has only
one repetition at 1.15% CPU. Vino's two idle means were 1.08% with normal copying
and 1.38% with batching; Vino sent no USB video data in any measured idle window.
There is no complete balanced idle comparison, and these figures do not
establish an idle advantage for batching.

### Throughput and power limits

All twelve active samples submitted 600 input images in approximately thirty
seconds without pacing warnings. Each Vino active sample encoded 599–603
changed updates, approximately twenty per second, and there were no throughput
warnings. Counts were independently checked against raw Vino events.
Required retransmissions are counted separately, so they cannot conceal an
obvious changed-update shortfall in this diagnostic.

This addresses the earlier obvious input/submission-rate mismatch at sixty
inputs per second. It still does not measure physical monitor FPS, latency,
exact coverage of input images, or official output cadence. The user was away
and provided no visual comparison for this unattended run. These results do
not establish full-screen 60-FPS performance or equal visual quality.

| Workload | Official AMD PPT | Vino normal AMD PPT | Vino batched AMD PPT |
|---|---:|---:|---:|
| Scrolling pattern | 16.43 W | 21.14 W | 11.72 W |
| Moving colours | 11.11 W | 12.07 W | 12.29 W |

These are observed processor/device sensor means, not wall power or isolated
driver power. Background CPU outside the display stack, player and sampler
averaged 0.97, 1.54 and 0.63 cores respectively during scrolling. The two
Vino/normal idle samples used almost identical driver CPU, yet their PPT means
were 15.41 W and 25.50 W, alongside differing background activity. This is
strong evidence that unrelated system activity remained a material confound.
The lower scrolling PPT and higher moving-colours PPT do not establish a
causal power, heat or fan improvement. Fan speed was not measured.

### Evidence and follow-up

All recorded CPU deltas, power integrations, selected batch settings and Vino
event counts were independently recomputed. The original source run is retained
unchanged, including the contaminated sample. Raw measurements, per-block
power/event records, journal evidence, desktop checks and reproducible analysis
are archived under **gpu-artifacts/comparison-evdi-20261020/run-toc_di9u/**.
The [analysis JSON](gpu-live-comparison-evdi-results.json) records all included
and excluded samples, sample counts and limits.

The active comparison is useful: the EVDI change reduced Vino's measured active
CPU in both workloads, while Vino's advantage over official was concentrated
in scrolling. The remaining goal is a dependable power comparison with quieter,
comparable background load, explicit screen-lock handling and visual output
assessment. The affected official idle repetition also needs replacement
before making a balanced idle comparison. Official with batching enabled has
not been measured; the shared EVDI change may benefit it too.
