# Documentation

The documentation is split by responsibility so protocol evidence does not
become mixed with kernel API design or build instructions.

| Document | Purpose |
|---|---|
| [`device.md`](device.md) | Supported hardware, heads, endpoints, modes, and known limits |
| [`architecture.md`](architecture.md) | Kernel ownership, control, KMS, scanout, and teardown invariants |
| [`protocol/usb.md`](protocol/usb.md) | USB endpoint map, transport header, framing, and I/O lifecycle |
| [`protocol/control.md`](protocol/control.md) | Plain initialization and encrypted control-plane structure |
| [`protocol/hdcp.md`](protocol/hdcp.md) | HDCP 2.2 message flow and verified key derivations |
| [`protocol/generations.md`](protocol/generations.md) | **★ The three generations side by side**: what every dock shares (crypto, framing, message vocabulary, the set-mode record), what differs per generation, and which four things are genuinely code rather than `DockProfile` data |
| [`protocol/navarro-decoded.md`](protocol/navarro-decoded.md) | The DL-7400 wire, decoded end to end: per-connector keys, stream open, mode set, strip parameter map |
| [`protocol/video.md`](protocol/video.md) | Video arm, codec, records, damage, and submission |
| [`adding-a-device.md`](adding-a-device.md) | **★ Start here for new hardware**: why product IDs are not how devices are identified, what the identity descriptor is, and what to send instead of a patch |
| [`new-device-capture.md`](new-device-capture.md) | Onboarding DisplayLink hardware vino cannot yet drive: descriptor triage, the gen-1 `init_4` divergence, and a capture procedure built to record a firmware update |
| [`new-device-day.md`](new-device-day.md) | **★ Runbook for the WAVLINK DL7400 (Navarro / DL-7000)**: the DFU shape predicted from descriptors, phase-by-phase commands, and what each capture closes |
| [`new-device-day-ella.md`](new-device-day-ella.md) | **★ Runbook for the HP 3005pr (Ella / DL-3900)**: disarming vino's auto-flash, the one-shot pre-flash session against the archived 2014 implementation, then DLM's first contact and the `DockProfile` a third generation needs |
| [`hdr.md`](hdr.md) | HDR/deep colour: the shared `NM30`/`YU10`/`FP16` format enum found in all three host binaries, the BT.2020 conversion path, and what vino would need |
| [`reverse-engineering.md`](reverse-engineering.md) | Evidence policy, capture method, independent oracles, and adding findings |
| [`ridge.md`](ridge.md) | **★ Dell D6000 / DL-6xxx**: the vendor bring-up decoded end to end, the shared EDID handler that makes a pre-engagement fetch describe the dock rather than the monitor, and the open endpoint stall with four refuted theories |
| [`navarro.md`](navarro.md) | **★ DL-7000 / DL7400 vs Ridge**: two video endpoints, no per-head HDCP auth, plaintext AKE framing, and the video stream-open sequence that Ridge's arm replaces |
| [`gpu-encoder.md`](gpu-encoder.md) | Offline Ella Vulkan encoder, correctness checks and CPU/GPU benchmark method |
| [`gpu-deep-optimization.md`](gpu-deep-optimization.md) | Fused GPU encoding, removed transfers and CPU copies, binary evidence and the next capture target |
| [`gpu-direct-capture.md`](gpu-direct-capture.md) | Native EVDI capture into mapped GPU storage, partial-update repair and the supervised trial |
| [`gpu-windows-shaders.md`](gpu-windows-shaders.md) | Reproducible Windows compute-shader inventory and observations |
| [`gpu-encoder-results.md`](gpu-encoder-results.md) | Measured prototype results and integration verdict |
| [`gpu-encoder-followup.md`](gpu-encoder-followup.md) | Linux analysis of the Windows shaders and revised packer measurements |
| [`gpu-official-comparison.md`](gpu-official-comparison.md) | Live DisplayLink service CPU versus the offline GPU prototype with controlled source frames |
| [`gpu-live-first-target.md`](gpu-live-first-target.md) | One-monitor live GPU trial, selective dock ownership and automatic DisplayLink recovery |
| [`gpu-live-comparison.md`](gpu-live-comparison.md) | Matched single-monitor live official/GPU comparison with CPU, AMD power and automatic recovery |
| [`gpu-live-comparison-results.md`](gpu-live-comparison-results.md) | First completed live comparison: CPU usage, mixed power results and throughput limits |
| [`gpu-live-comparison-evdi-results.md`](gpu-live-comparison-evdi-results.md) | Official/Vino/EVDI comparison at 20 inputs per second, with the lock-affected idle sample excluded |
| [`gpu-live-power-comparison-results.md`](gpu-live-power-comparison-results.md) | Monitored power comparison: clean recovery and smooth output, with scheduled background work identified |
| [`gpu-live-cron-off-results.md`](gpu-live-cron-off-results.md) | Comparison with ZiRan cron disabled: scrolling CPU improved, with no demonstrated power or heat benefit |
| [`gpu-live-direct-comparison-results.md`](gpu-live-direct-comparison-results.md) | Current direct-capture build versus proprietary: CPU wins, power observations and the scrolling kernel-CPU regression |
| [`gpu-usb-reuse.md`](gpu-usb-reuse.md) | Reused packet buffers and USB transfer objects, cancellation safety and component results |
| [`gpu-shared-capture.md`](gpu-shared-capture.md) | GPU strip comparison and experimental shared EVDI framebuffer capture |
| [`gpu-native-revisions.md`](gpu-native-revisions.md) | Exact native strip comparisons, retained revisions and removal of full-screen hashing |
| [`gpu-idle-results.md`](gpu-idle-results.md) | Duplicate-capture filtering, event-driven waits, CPU measurements and live power diagnostics |
| [`gpu-cpu-profile-results.md`](gpu-cpu-profile-results.md) | Live CPU profile identifying capture and image hashing costs |
| [`gpu-retransmit-hash-results.md`](gpu-retransmit-hash-results.md) | Reusing hashes for unchanged retransmissions, correctness checks and component CPU measurements |
| [`gpu-evdi-copy-investigation.md`](gpu-evdi-copy-investigation.md) | EVDI row-copy batching prototype, isolated kernel benchmark and hardware validation boundary |
| [`gpu-evdi-live-trial.md`](gpu-evdi-live-trial.md) | Temporary EVDI module switch, recovery and profiled desktop batching trial |
| [`simd.md`](simd.md) | AVX2 in the encoder, measured in the kernel: 2x on the transform written within a block, why the across-blocks form regresses, and why the entropy coder caps the whole question |
| [`color-management.md`](color-management.md) | Software `CTM`/`GAMMA_LUT` on the CRTC, shared by vino and evdi: pipeline order, the fused fast path, and the sign-magnitude trap |
| [`revdi-chimera.md`](revdi-chimera.md) | Revdi ABI compatibility, Chimera data flow, and its DLM-shaped damage path |
| [`upstream.md`](upstream.md) | Current bases, v1/v2 feedback disposition, authorship, and subsystem ownership |
| [`testing.md`](testing.md) | Build-only validation, patch reproduction, examples, and hardware test boundary |
| [`handover.md`](handover.md) | **★ START HERE: current state and what is open** — the single live handover, with the run procedure, the offline oracles, and the settled-do-not-re-chase list |

The kernel-facing user documentation is also present in
`linux/Documentation/gpu/vino.rst`. It intentionally documents the supported
production behavior, not the history of discovering it.
