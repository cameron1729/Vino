# GPU prototype handover

Checkpoint: 2026-10-22. The objective is a smooth DisplayLink desktop with less
CPU work. Lower laptop power and heat are separate outcomes to measure, not
automatic consequences of moving encoding to the GPU.

## Repositories and branches

| Repository | Working branch | Role |
|---|---|---|
| [cameron1729/Vino](https://github.com/cameron1729/Vino/tree/gpu-encoder-prototype) | `gpu-encoder-prototype` | Userspace GPU driver, capture integration, trial/recovery tools and results |
| [cameron1729/evdi](https://github.com/cameron1729/evdi/tree/gpu-shared-capture) | `gpu-shared-capture` | EVDI C module: copy batching and experimental shared framebuffer leases |

Both repositories use the owner's SSH remote as `origin` and default push
destination. Vino is a standalone public preservation of the local history;
`FireBurn/Vino` was unavailable through the API and Git on the checkpoint date.
Its old remote remains as `upstream` for provenance. EVDI is a GitHub fork of
`DisplayLink/evdi`; its feature branch starts at upstream `v1.15.1`, commit
`33b7b3c9f7c77157c2b1b0c5a40cebe19b8ba110`.

The local checkouts are `~/Projects/Vino` and `~/Projects/evdi`. A fresh clone
defaults to `main`; the work and this handover are on the branches above.
The historical `linux/` submodule was never initialized here and its source was
not backed up. It is not required by the userspace GPU prototype.

## Current implementation and evidence

The active implementation is in `revdi/gpu-encoder` (Vulkan encoding, comparison
and DMA-buf copying), `revdi/chimera` (capture, cursor, revisions and scanout),
`revdi/library` (EVDI client) and `revdi/vino-driver/src/usb.rs` (USB transport).
`tools/gpu/` contains supervised runners, recovery, benchmarks and EVDI patches.
The older [kernel handover](handover.md) describes a separate upstream effort.

Direct capture, CPU native revisions and reusable USB packet/transfer storage
have had smooth live desktop trials. Earlier matched official/Vino comparisons
and subsequent component profiles are already preserved; use those findings to
choose concrete work rather than repeating a comparison without a new question.
See [direct capture](gpu-direct-capture.md), [USB reuse](gpu-usb-reuse.md),
[native revisions](gpu-native-revisions.md) and the
[direct-capture comparison](gpu-live-direct-comparison-results.md).

The newest additions are GPU strip comparison and shared compositor framebuffer
capture. Private GPU/oracle checks, userspace checks, isolated kernel builds and
47 host fault checks passed. **The combined shared capture/GPU comparison path
has not had a live compositor/display trial.** Default comparison remains CPU;
the new paths are explicit options. Details, constraints, build identities and
the existing supervised procedure are in [shared capture](gpu-shared-capture.md).
The [preserved results](results/README.md) distinguish live observations,
component measurements and private-memory checks. This remains a prototype.

## Next work

At this checkpoint the user is away from the DisplayLink dock. The next hardware
milestone is the first bounded combined shared-capture/GPU-comparison trial when
the dock is available again, including compositor lease/recovery behavior.
The shared-capture document contains the prepared procedure. Assess remaining
CPU work from that path once its live correctness is established.

Archived module identities, absolute paths and reports describe past runs.
Confirm the current kernel, attached hardware, loaded EVDI and display services
before resuming hardware work. A successful build does not mean the candidate
is installed or loaded; rebooting restores the installed module after a
temporary switch. Existing runners handle bounded trials and recovery; module
switches sign out the desktop and require the user's terminal for sudo.

## Keeping the two repositories usable

Vino's `evdi-copy-batching.patch`, `evdi-shared-capture.patch` and
`evdi-shared-capture.h` under `tools/gpu/` mirror the companion EVDI source.
Keep them consistent when changing either side. `prepare-evdi-shared.py` expects
an **unpatched** EVDI module source directory and applies batching before shared
capture; the already-patched EVDI branch is not a suitable input for that script.

Git contains source, test tools and selected reports, not `gpu-artifacts/`, Rust
targets, compiled modules, proprietary DLLs or extracted shaders. Rebuild on a
new machine with the pinned Rust toolchain and `glslc`; native CPU binaries and
kernel candidates are machine-specific. The Windows DLL is optional research
input and is not required to build or run the encoder. Build guidance starts in
[the GPU encoder document](gpu-encoder.md); the live worker uses the `live` Cargo
feature. Kernel candidates must match the current kernel and original module.
