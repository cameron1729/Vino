## First target

The opt-in `vino-gpu-live` binary feeds one Ella dock output through the existing Vulkan encoder. It can send a deterministic test pattern or capture a KDE/X11 desktop through the installed EVDI module. It uses 1920×1080 SDR at a 60 Hz link rate, with encoded desktop updates capped at 60 per second. Desktop capture runs independently and replaces a single pending image with the newest complete snapshot. Encoding and USB remain synchronous with one frame in flight; there is no frame backlog or automatic CPU fallback.

This milestone establishes a usable live display and reliable recovery. It does not establish a performance, latency, power or temperature improvement over DisplayLinkManager.

### Current validation

- The live release build and read-only hardware preflight pass on the Radeon 780M, EVDI 1.15.1 and USB `17e9:4307` at `8-1.2`
- Full offline GPU verification passes: 160 surfaces, 16,520 strips and 64 framed heads, with zero Vulkan validation errors
- Rust tests cover encoder failure, partial USB submission, sequence wraparound, framebuffer geometry/format/stride, selective USB descriptors and delayed EVDI replies
- Eighteen Python tests cover monitor identity/layout restoration, diagnostic startup/readiness, surfaced worker errors, write-ahead compositor restoration and independent recovery failures, including xrandr’s global `--noprimary` behaviour
- Seven temporary user-systemd trials cover normal exit, early failure, startup hang, crash, watchdog hang, runtime expiry and interruption; all restore their simulated state, including cleanup lasting longer than the worker's two-second stop deadline
- Strict Clippy passes for the GPU package with `--no-deps`; applying `-D warnings` to all Chimera/transport code encounters existing warnings in the vendored codec and upstream utilities
- The first hardware trial authenticated the Ella control session, then failed to read head 0 EDID before sending pixels; DisplayLink and both monitors returned, as confirmed by the user
- That trial exposed a timeout-extension failure during cleanup after a pre-readiness exit; recovery now runs in its own bounded service, with delayed-cleanup tests for startup failures
- The initial launch issue on the `noexec` **/run** mount is fixed by staging and checking the executable on **/var/tmp**
- Subsequent CPU trials again returned no EDID and submitted zero frames; both monitors recovered, and the independent recovery service completed
- Recovery’s per-output use of the global `--noprimary` option is corrected; the laptop panel’s original primary setting is restored
- Chimera’s Ella setup now follows native `session/setup.rs`: content-stream announcements after each Stream_Manage, no connector finalizers, one vendor commit, probe/fetch across both connectors before engagement, and the two sealed stream opens with the profile’s 2/3/3 status-poll spacing
- The decoder configuration continues each stream’s setup counter; mode packets use the counter after their preceding markers, and the carrier count includes the first frame
- The next CPU trial (`/var/tmp/vino-live-lssufvp5`) successfully read both monitor EDIDs and submitted six frame batches, then hit an EP02 stall; the user still saw no signal, and recovery restored all displays and the primary setting
- The current build also removes Ella’s unsupported post-carrier `0x16/0x4c` commits and per-frame status polls, uses only the profile’s periodic status keepalive, spaces repeated presentations by one frame period, and applies native frame-tail splitting
- Subsequent CPU trials still produced no signal: **/var/tmp/vino-live-r6k7ysd6** submitted eight frame batches at 10 updates/second and **/var/tmp/vino-live-97zld5ln** submitted seven at one update/second before EP02 stalled; both restored the original displays
- Firmware diagnostics in the one-update/second trial show that the dock accepted the 1080p mode and enabled head 0, but every SetBlanking command was on; the two unsolicited 0x03/0x82 replies are documented downstream-state notifications
- Ella activation now releases blanking after the carrier and two successful picture presentations, following the release point in native ELLA_DOCK_WIDE, then reasserts power state 0; the previous per-head path omitted that release entirely
- The code before the cursor change passed 41 Chimera tests, including unblank ordering, injected frame/marker failures, cadence under delayed wakeups and framing equivalence against the original native path
- The corrected CPU trial **/var/tmp/vino-live-d8ycgekq** completed the full ten seconds at one update/second: 11 frame batches, 13 presentations, no USB error and successful automatic restoration; firmware confirmed head 0 blanking off, and the user confirmed an image on the Dell and both desktops restored
- The GPU pattern trial **/var/tmp/vino-live-op1dtfi9** completed its full minute with a 10-update/second cap: 595 frame batches, 597 presentations, 1,190 GPU dispatches and 29,076 encoded strips; there were no reported Vulkan validation errors or video submission failures, the user confirmed an image, and recovery reported success with DisplayLink active
- The first GPU desktop trial **/var/tmp/vino-live-njbdwr58** submitted 527 frame batches before the user stopped it after about 30 seconds because movement was steadily jerky; encoding took 2.26 ms at the median among submitted frames, while most movement reached only 29 updates/second under the original 30-FPS cap; the process and recovery both completed successfully, with no reported Vulkan errors
- Desktop updates now default to a 60-FPS cap with deadlines on a fixed cadence; late wakeups no longer accumulate drift, and missed slots are skipped without queuing old frames
- The first 60-FPS desktop trial **/var/tmp/vino-live-wlrkcu5t** completed the minute with 3,329 submitted batches, 4,434 GPU dispatches, no reported Vulkan errors and successful restoration; the user reported smooth cursor movement but jerky Konsole dragging
- Large updates in that run (at least 250 kB) took 17.43 ms at the median and 22.45 ms at the 90th percentile across capture, encoding/framing and USB, exceeding the 16.67 ms budget for some refreshes; USB completion does not establish monitor presentation
- Scanout now preorders Ella strips with a stable cached-key sort using the native band table before passing them through the unchanged native framer; this avoids its quadratic insertion-sort cost while preserving exact wire bytes for full frames, sparse updates, duplicate coordinates and unlisted bands
- An offline constant-frame packing comparison measured 6.56 ms before versus 0.44 ms after for 2,040 strips; three alternating full scrolling comparisons increased two-head throughput from 23.85–25.63 to 33.11–33.94 frame pairs/second with identical byte/strip counts (**gpu-artifacts/live-framing-gi38e15g/comparison**); these exclude capture and USB
- Full GPU verification passed again after the framing change: 160 surfaces, 16,520 strips, 64 framed heads and zero validation errors (**gpu-artifacts/live-framing-gi38e15g/verification**)
- The next 60-FPS desktop trial **/var/tmp/vino-live-4u8cx_8z** completed with 2,658 submitted batches, 3,504 GPU dispatches, 273,828 encoded strips and successful restoration; large updates after startup took 13.76 ms at the median, but the user still reported jerky Konsole movement
- The user found that moving the same window mostly onto the Dell was smooth while the pointer stayed on the laptop panel; dragging with the pointer on the Dell was jerky, which suggests a cursor/capture interaction but does not yet prove its cause
- EVDI 1.15.1 marks cursor movement as framebuffer damage in its automatic composition mode, and can defer page-flip completion until GRABPIX; the trial previously used this mode, so submitted frame counts could include cursor-only movement
- The optional `--cursor client` path requests separate EVDI cursor events and composites the latest cursor over a clean desktop image in the client; logs separate cursor-event counts, framebuffer damage and composition time
- The separate cursor path passes 45 Chimera tests, including movement, hiding, image replacement, alpha blending, padded stride, clipping, hotspot coordinates and malformed buffers; all four library tests pass, including explicit detection of cursor-ioctl and bitmap-mapping failures, and the rebuilt binary passes Clippy and read-only GPU preflight
- The client-cursor trial **/var/tmp/vino-live-81u0qvd1** completed its minute with 2,040 submitted batches, 2,210 captures and successful restoration, but received zero cursor events despite a successful enable ioctl and no reported fallback; every capture had full-screen damage and the user reported the same pointer-location-dependent stutter
- A subsequent read-only DRM query with the pointer on the Dell found all cursor planes inactive, including the Dell's plane; this supports X11 software composition upstream of EVDI, so the separate EVDI cursor path did not isolate cursor rendering on this session and `--cursor evdi` remains the default
- The KWin compositor-off trial **/var/tmp/vino-live-c6zyah54** submitted 753 batches over approximately 53 seconds after readiness and restored both the compositor and display layout successfully; the user reported the same smooth movement with the pointer on the laptop and stutter with it on the Dell, so suspension did not resolve the problem
- The optional `--trace-motion` diagnostic records X11 root-child window geometry events and pointer coordinates on raw input, with monotonic timestamps shared with frame logs; frame logs now count newly changed strips separately from retransmit debt, so submitted batches cannot be mistaken for new window positions
- Its first live run **/var/tmp/vino-live-fp8r9m5u** failed after 4.62 seconds with zero submitted frames: the synchronous layout hook timed out waiting for the X11 tracer to initialize, and recovery restored the normal layout successfully
- Diagnostic startup now runs in the worker's initial phase, before connecting EVDI, instead of waiting for a new X11 client after enabling the virtual output; startup logs identify the last initialization step, and launcher errors include the worker's failure reason
- The successful traced trial **/var/tmp/vino-live-_xz2gh0g** completed its minute with 1,566 submitted batches and successful restoration; the user again reported smooth movement with the pointer on the laptop and jumpy movement with it on the Dell
- During a 5.87-second laptop-pointer drag, the trace recorded 594 geometry changes for the moving window and 330 updates with at least 128 changed strips, with a 16.80 ms median interval between those substantial image updates; a subsequent 2.81-second Dell-pointer drag recorded only five window geometry changes and six substantial image updates, despite a 4.91 ms median capture/encode/USB duration
- X11 pointer queries during that Dell interval sometimes took over half a second, versus approximately 0.33 ms at the median in the smooth interval; this locates the observed slowdown upstream of encoding, although it does not by itself identify the blocking kernel/Xorg call
- The capture worker now collects EVDI updates independently of the output timer and GPU/USB work, so EVDI's capture-dependent flip acknowledgements do not wait for that work; the device is opened, used and closed on its owning thread, and the main loop checks a capture heartbeat before keeping the external watchdog alive
- A single replaceable pending image and recycled owned buffers let capture continue while encoding holds another image; replaced snapshots are counted instead of queued, and non-image events and DDC/CI replies have bounded queues
- The capture change passes 49 Chimera tests, including a producer completing 999 updates while the consumer holds its previous image, three-buffer reuse, capture-stall/error detection and joined shutdown with error propagation
- Two subsequent one-minute trials, **/var/tmp/vino-live-m4q5nuf1** and **/var/tmp/vino-live-vokagqj4**, completed successfully with independent capture; the user confirmed smooth movement
- In the latter run, a 1.22-second Dell-pointer drag recorded 86 geometry changes for the moving window and 46 image updates with at least 128 changed strips, at a 16.65 ms median interval; pointer queries across the run took 0.32 ms at the median and 2.07 ms at the 99th percentile, supporting the improvement without establishing monitor presentation timing
- The user reported the BenQ missing after restoration; both runs recorded successful recovery approximately 4.6 seconds after worker exit, and the second trial started approximately 1.3 seconds after the first recovery completed, temporarily disabling the BenQ again; subsequent read-only EDID/layout checks found the BenQ, Dell and laptop active in their saved positions, and the user subsequently confirmed that all displays were back to normal
- The 15-minute GPU desktop trial **/var/tmp/vino-live-btnklf4h** completed successfully with Vulkan validation enabled; the user reported smooth movement throughout, including YouTube playback on the Dell and window dragging, and the recovery log reported successful restoration
- That run submitted 50,408 frame batches from 63,239 captures and replaced 16,551 pending captures before encoding; process CPU time was 654.91 seconds over 902.41 seconds elapsed, equivalent to 0.726 logical processors on average, excluding the browser, Xorg and KWin
- The user reported increased fan activity during the long run; total power, temperatures and a matched official-driver workload were not recorded, so the cause of the fan activity and any efficiency advantage remain unestablished; capture currently converts every complete image on the CPU before publication, including images later replaced, which is a profiling candidate
- Offline profiling did not justify replacing RGB conversion with a native-format copy: conversion took 0.47–0.56 ms per image across three component samples, versus 0.51–0.60 ms for copying the larger native image; neither includes capture ioctls, GPU work or USB
- Investigation instead found that `wait_readable` truncated a positive fractional-millisecond timeout to zero; `try_next_frame` then repeatedly called `poll(0)` until its two-millisecond deadline, consuming CPU while waiting for updates
- The fix rounds positive timeouts up to milliseconds while preserving explicit zero-timeout checks and immediate wakeup on incoming events; an isolated empty-socket reproduction using the actual old and new helpers measured approximately 47.5% versus 0.5% of one CPU core across three alternating runs, with over two million versus 971 poll calls per two-second sample
- The fractional-timeout regression test failed before the fix and passes afterward; all seven library tests and 49 Chimera tests pass, and the rebuilt GPU package passes strict Clippy and read-only hardware preflight; [raw measurements and artifact fingerprints](gpu-capture-wait-results.json) distinguish the component results from complete-driver efficiency
- The subsequent three-minute live trial **/var/tmp/vino-live-1km11gf2** completed successfully with restoration; process CPU averaged 0.384 logical processors versus 0.726 in the earlier 15-minute run, an observed reduction of approximately 47%, with Vulkan validation enabled in both runs
- USB byte rates from the final health records were approximately 18.76 MB/s after the fix and 18.77 MB/s before; submitted frame-batch rates were 57.2 and 55.9 per second respectively, so these logs do not suggest lower submitted throughput as the explanation for lower CPU use; they do not measure monitor presentation
- The user thought the new trial was better but still noticed the fan; the runs differed in duration and were not an identical workload replay, and neither measured total power, temperatures or fan speed, so the live CPU reduction is an exploratory observation rather than a controlled power comparison
- The [first completed live comparison](gpu-live-comparison-results.md) ran all twelve official/GPU samples and restored the normal layout; the user reported smooth output, measured Vino CPU was lower in active samples, and power readings were mixed; equal delivered cadence and image-quality equivalence remain unverified
- The [idle follow-up](gpu-idle-results.md) filters identical RGB snapshots before hashing unless retransmissions remain owed, and replaces two-millisecond polling with event-driven waits; offline component measurements and regression checks pass; its two-minute desktop trial completed with restoration and the user confirmed movement remained smooth, with about 5.9% of one CPU core during a quiet subwindow
- **CPU and GPU output, smooth window movement, video playback and 15-minute desktop stability are confirmed on the Dell, and recovery of the normal three-screen layout is confirmed; the latest idle changes also passed a two-minute live check, while a matched live CPU/power reduction and power/heat improvement remain unverified**

The official DisplayLink service remains the installed driver. No firmware, kernel module, global Rust setup or system DisplayLink library is replaced.

## Build and preflight

From the project root:

```bash
RUSTFLAGS='-C target-cpu=native' cargo build --manifest-path revdi/Cargo.toml -p vino-gpu-encoder --features live --release --locked
python3 tools/gpu/live-trial.py preflight --validation
```

Preflight checks the service, laptop panel, existing EVDI card, dock identity in sysfs and Vulkan initialization without claiming USB or opening an EVDI session. The live transport then validates the identity descriptor and interface layout again before ownership changes.

Defaults are physical USB port `8-1.2`, dock head 0, existing DRM card index 0 and Vulkan device index 0. Override these explicitly if enumeration or cabling changes. The first trial deliberately requires the installed EVDI version 1.15.1 and an active, unmasked DisplayLink service. It refuses an existing runtime override, a missing laptop panel or a rotated layout.

The GPU feature is opt-in. Existing `vino-gpu` benchmark/verification commands still operate offline.

## Controlled hardware tests

Both dock displays may go blank while ownership changes. The laptop panel remains primary. Pattern trials last one minute at 10 updates/second; the desktop trial lasts 15 minutes at up to 60 updates/second. Successful USB submissions must be confirmed visually: a submission timestamp does not prove monitor presentation.

1. Run the CPU pattern and **verify** coloured bars, a black-and-white checkerboard and a moving orange block on one external monitor, followed by restoration of both original displays
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 tools/gpu/live-trial.py run --encoder cpu --source pattern
    ```
2. After the CPU trial succeeds, run the same pattern through Vulkan and **verify** matching colours, geometry, motion and restoration
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 tools/gpu/live-trial.py run --encoder gpu --source pattern --validation
    ```
3. After both patterns succeed, run the captured desktop and **verify** readable text, a visible cursor, selection, scrolling, window movement and a stable image when idle
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 tools/gpu/live-trial.py run --encoder gpu --source desktop --validation
    ```
4. Repeat a successful pattern trial, interrupt it with Ctrl-C and **verify** both displays return to their original arrangement
5. Across at least three takeovers, **verify** dock audio/network bindings remain unchanged and each trial reports successful restoration

The runner prints its directory under **/var/tmp/vino-live-…**. Keep **events.jsonl** and **recovery.json** with the visual observations. These directories are retained for inspection after the trial. `frames_submitted` counts successful frame batches, while `presentations` counts the copies required for the dock's buffers. Encoding timing includes planning/framing; USB timing includes submission framing and transfer completion. Health records report process CPU seconds, cumulative submitted bytes, all completed captures and `superseded_captures` replaced before encoding. GPU totals include actual dispatch and strip counts, proving which backend ran.

With the optional `--cursor client`, cursor-only movement can submit a frame without capturing the desktop, provided X11 supplies a separate cursor plane. Frame records distinguish `captured`, `cursor_events`, `damage_rects`, `damage_pixels` and `cursor_composite_ms`; damage pixels sum rectangle areas and can count overlaps twice. Health records total captures and cursor events separately. Neither capture counts nor successful USB submissions establish visible window FPS. The default remains `--cursor evdi` because the current X11 session delivered no separate cursor events during the comparison.

For a compositor comparison, add `--no-compositor` to a short desktop trial. This requires KWin X11 and temporarily suspends compositing, so effects and transparency change on the laptop too. Recovery resumes compositing only if it was active before the trial. It records the restoration obligation before suspension, checks the resulting state, and continues display recovery even if compositor restoration fails.

For movement diagnosis, add `--trace-motion` to a short desktop trial and **verify** the difference between dragging wholly on the Dell and moving most of the same window onto it while the pointer remains on the laptop. `motion.jsonl` records X11 geometry-event receipt times and pointer positions without window titles or pixel contents. The trace starts before the worker connects EVDI, runs as the desktop account within the trial service's cgroup, and stops with the trial before recovery. Its readiness requires a `start` event after X11 subscription; initialization output alone does not satisfy it. It subscribes to events instead of polling idle X11; pointer query duration is logged because the diagnostic itself can affect scheduling. Geometry events show requests processed by X11, not compositor rendering or monitor presentation.

Frame records include `changed_strips`, strip-aligned `changed_bounds`, and `monotonic_ns` at log emission after processing. These changes compare against the last successful presentation and exclude retransmit-only work; they do not classify cursor versus window pixels. `capture_sequence` identifies the selected snapshot and `capture_age_ms` is its age when the encoder took it. `capture_ms` measures that snapshot's capture and conversion on the worker, including any wait for readiness; it now overlaps other work and must not be added to encoding/USB time as a sequential frame budget. The trace itself is diagnostic and does not change capture pacing, encoding or the wire format.

Acceptance requires the 15-minute desktop test and repeated restoration, not just the automated checks. CPU/power comparison against DisplayLinkManager follows after this milestone.

## Ownership and recovery

The selected Ella opener claims interface 0 for display transport and interface 1 for runtime control. It never detaches kernel drivers, changes USB configuration, claims the audio status endpoint on interface 2, resets the composite device or uploads firmware. Audio interfaces 2–4 and Ethernet interfaces 5–6 retain their original drivers.

The trial links the Rust EVDI client privately. Its capture thread opens an existing selected card, selects its cursor policy before connecting, and validates 32-bit XRGB/ARGB8888 capture before conversion. Only owned RGB buffers and events cross threads; the EVDI handle never does. The optional separate cursor path validates cursor bitmaps and restores a clean desktop image before each composition so moved or hidden cursors leave no trails. EVDI cursor coordinates already identify the bitmap's top-left; the client does not subtract the hotspot again. In that mode the trial also stops if the library cannot enable or map separate cursor events, rather than silently treating its automatic composition fallback as a successful comparison. Capture ioctl failures propagate; a request that times out keeps its eventual reply rather than issuing duplicate requests. A stopped or unresponsive capture thread fails the trial, and external supervision still bounds a stuck ioctl during shutdown.

Before stopping DisplayLink, the runner copies the helper and binary into a root-owned directory on the executable **/var/tmp** filesystem and installs a transient systemd service with recovery already attached. It saves output positions, refresh rates, primary output and monitor EDIDs, then runtime-masks DisplayLink to prevent its restart policy or udev from racing USB ownership.

The service allows 30 seconds for startup, requires progress from the actual main loop every five seconds after readiness, and terminates a stuck worker after a two-second grace period. The runtime limit remains effective if the launching terminal disappears. The `ExecStopPost` hook starts a separate recovery service with a 45-second startup limit; this also works when the worker fails before readiness. Recovery removes only the trial's runtime mask, restarts DisplayLink and restores the saved layout using EDIDs if output names changed. A recovery error is reported separately from an encoder or transport failure.

Ctrl-C requests supervised shutdown. If the launching terminal is gone, `systemctl list-units 'vino-trial-*'` identifies the running trial; stopping that exact unit invokes the same recovery. The runner never uses `remove_all`, unloads EVDI or reauthorizes the whole dock.

## Automated checks

```bash
RUSTFLAGS='-C target-cpu=native' cargo test --manifest-path revdi/Cargo.toml -p vino-gpu-encoder -p vino-chimera -p vino-driver --features vino-gpu-encoder/live --release --locked
RUSTFLAGS='-C target-cpu=native' cargo test --manifest-path revdi/library/Cargo.toml --release
RUSTFLAGS='-C target-cpu=native' cargo clippy --manifest-path revdi/Cargo.toml -p vino-gpu-encoder --features live --all-targets --no-deps --release --locked -- -D warnings
python3 -m unittest discover -s tools/gpu -p test_live_trial.py -v
python3 tools/gpu/live-trial.py self-test
revdi/target/release/vino-gpu verify
```

The supervision self-test needs a running user systemd manager. It starts disposable mock processes, never accesses display devices, and never stops DisplayLink.
