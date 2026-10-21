# Exact native change detection

Direct desktop capture now compares logical RGB pixels per strip and retains
a revision for each unchanged strip. Planning uses those small revision values
instead of hashing the full image after every pixel or cursor change. Copied
capture keeps its existing hash path.

The comparator scans rows in memory order within each strip-height band and stops
reading a strip when it finds a difference. It ignores unused X bytes and row
padding. Only strips intersecting the old or new cursor need composited-pixel
comparison. Cursor-only updates reuse the immutable background and skip reads
outside those footprints. EVDI damage hints never establish pixel equality.

After a capture compares unchanged, another capture first gets a sequential
logical-image equality probe. Duplicates avoid strip-mask bookkeeping; the first
changed capture falls through to strip comparison and can read a prefix twice.
Continuously changing inputs skip the probe. Same-backing cursor-only updates
also skip it, keeping their comparisons confined to the cursor footprints.

The tracker describes the most recently consumed capture and cursor snapshot.
The planner separately tracks what reached the dock, retransmission debt and
encoded strip bodies. Failed encoding or USB delivery cannot commit a new dock
state. Replacing the tracker, resizing, revision rollover or switching between
hashes and revisions invalidates the relevant cache identities.

An A → failed B → A sequence can conservatively resend A, because A now has a
new revision. The bytes are correct; revisions identify consecutive unchanged
content rather than recognizing every image seen previously.

The consumer retains both capture leases for comparison, then releases the old
lease before GPU encoding or USB waits. Capture acknowledgements, the four-slot
pool and display recovery remain on the existing paths.

## Validation

The tests compare native output against independently composed RGB and the CPU
codec across Ella, Ridge and Navarro strip geometries. They cover cursor movement,
clipping, opacity and hidden-background changes; X/padding differences; encoding
and USB failures; overlapping retransmission debt; cache reuse; resize and tracker
replacement.

The private-memory replay runs the old direct path and native revisions against
the same inputs and GPU encoder. It checks framed output fingerprints, byte counts
and encoded-strip counts outside timing. Its CPU totals include simulated capture
writes, comparison, planning, GPU submission/readback and framing, but exclude
EVDI kernel work, the compositor, USB transport and the physical display.

All **81 library tests and five native-revision integration tests passed**;
two manual component benchmarks were left ignored. The ten surface tests also
passed in both default and native release builds. Independent review covered
the comparison, revision domains, transactional planner and capture lease handling.

## Private-memory results

Radeon 780M, native release build, 1920×1080 input padded to 1088 rows, 16 warmup
frames and 96 measured frames per sample. Three rounds reversed path order on
the middle round. The table gives median process CPU milliseconds per frame,
comparing the previous direct path with the new direct revisions path:

| Input | Previous direct | Native revisions | CPU reduction |
|---|---:|---:|---:|
| Repeated identical captures | 0.641 | 0.682 | −6.5% |
| Small changing region | 1.114 | 0.317 | 71.6% |
| Scrolling | 4.079 | 2.853 | 30.1% |
| Moving colours | 2.641 | 1.470 | 44.3% |
| Full-screen noise | 4.838 | 3.683 | 23.9% |
| Cursor movement with repeated background captures | 1.217 | 0.842 | 30.8% |
| Cursor movement without new background captures | 0.441 | 0.020 | 95.5% |

All output-equivalence checks passed. The direct paths recorded zero desktop
upload and snapshot bytes. The cursor-only case recorded zero measured capture
writes or repair work.

Repeated identical captures cost 0.042 ms more in this replay; this case is
not a quiescent desktop with no capture events. The sequential duplicate shortcut
avoids the larger strip-mask overhead seen without it. These observations do not
establish a live idle-power improvement. Active-work gains appeared in all three
rounds, but these fixed inputs do not predict an exact whole-driver CPU saving or
laptop temperature change.

The final worker built successfully and its read-only preflight passed on the
Radeon 780M with four mapped capture slots. The physical desktop trial subsequently
passed, as recorded below.

Raw results, the summarizer, source/build hashes, exact binaries and preflight are
in **gpu-artifacts/native-revisions-20261021/**. The worker SHA-256 is
`40e3f25607490ac0e7f5f7e3e8d0516d02288b8efff0ca632066080c8bf8795b`.
Exploratory results are retained separately; **replay.json** and **summary.json**
describe the final build.
The final [replay samples](results/native-revisions-20261021/replay.json) and
[summary](results/native-revisions-20261021/summary.json) are also preserved with
the source; compiled binaries and raw profiles remain local.

To reproduce the private-memory comparison:

```bash
revdi/target/release/vino-gpu --device 0 capture-bench \
    --frames 96 --warmup 16 --rounds 3 \
    --output gpu-artifacts/native-revisions-20261021/replay.json
python3 gpu-artifacts/native-revisions-20261021/summarize.py
```

## Completed live trial

The two-minute profiled run **/var/tmp/vino-live-olt55681** completed 120.016 seconds
of desktop operation successfully. The user reported smooth output. Recovery had
no errors, restored EVDI batching to zero and returned the original desktop layout.
An independent check matched the laptop, Dell and BenQ by EDID, position, mode,
primary flag and 60-Hz refresh; DisplayLink and KWin were active and the dock's
audio/network bindings were unchanged. The captured worker hash matches the build
above.

The run completed 6,719 captures, 6,248 submitted frame updates and 3,759 GPU
dispatches, all direct, sending 530.32 MB over USB. Desktop pixel uploads, RGB
conversion, snapshots, repair bytes and pool exhaustion remained zero. Peak capture
leases were three of four. Cursor texture uploads were 1,196,032 bytes, as expected
for the separate cursor path.

Between first and last health samples, 14.325 CPU seconds over 119.436 wall seconds
give **11.99% of one CPU core**. The context from the preceding manual trial is:

| Measurement | Previous USB-reuse trial | Native-revision trial |
|---|---:|---:|
| Total worker CPU, percent of one core | 14.94 | 11.99 |
| Main-thread CPU, percent of one core | 9.68 | 6.79 |
| Capture-thread CPU, percent of one core | 5.25 | 5.21 |
| Captures over the full trial | 5,538 | 6,719 |
| Submitted updates over the full trial | 5,106 | 6,248 |
| USB bytes over the full trial, MB | 230.56 | 530.32 |

CPU was lower while this run handled more captures and bytes. These were freely
used desktops with different activity, so the observed reduction is not a matched
before/after percentage or evidence of a particular heat/power saving.

The CPU profile confirms that the old full-screen hashing hotspot is absent from
the direct path. The remaining sampled work is:

| Work | Share of sampled worker CPU |
|---|---:|
| EVDI capture ioctls: copies, cache flushing and checks | 41.07% |
| Exact native change detection, including duplicate probes | 37.02% |
| Userspace memory copies | 2.06% |
| Other CPU work | 19.84% |

Planning itself accounts for 0.32% of sampled CPU, within the last row. The profile
has 2,812 samples, no reported lost samples and no unknown leaf symbols. Their
periods represent 14.131 CPU seconds, within 1.36% of the process counter. Some
caller stacks stop early, so generic memory-copy samples remain unassigned rather
than being attributed to a specific operation.

Captured-frame age was at most 16.66 ms (p95 14.36 ms); after the initial frame,
the largest recorded encode and USB stages were 3.41 and 11.38 ms respectively.
These are processing observations, not measured display latency or rendered FPS.
The mean reported AMD sensor reading was 23.44 W; it covers the whole AMD device
and cannot identify Vino's power consumption in this manual workload.

The **live-olt55681/** artifact subdirectory contains the raw profile, exact worker,
kernel symbols, exported reports, reproducible **analyze-profile.py**,
**profile-summary.json**, **trial-summary.json** and the independent
**desktop-recovery-check.json**.
The published [profile summary](results/native-revisions-20261021/profile-summary.json)
and [trial summary](results/native-revisions-20261021/trial-summary.json) preserve
the measurements, sampled CPU attribution and their limitations.

## Live check

1. Run the bounded desktop trial in a terminal, drag and scroll on the Dell, play video and **verify** smooth output and correct cursor movement:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run \
        --encoder gpu --source desktop --capture direct --cursor client \
        --evdi-copy-rows 128 --fps 60 --duration 120 --profile-cpu
    ```
2. Let it finish and **verify** both desktops return with `restored: true` and no recovery errors

The runner restores the original layout and EVDI batching automatically. Ctrl+C
starts restoration early. Profiling is for locating remaining CPU work; it adds
overhead and is not a matched power comparison.
