# GPU comparison and shared framebuffer capture

This prototype addresses the two remaining desktop CPU hotspots identified in
[the native-revision profile](gpu-native-revisions.md): exact pixel comparison
and the EVDI copy into the capture pool. GPU comparison and shared capture are
independent options. Their private-memory checks have passed; the new shared
capture path has not yet been validated against the live compositor.

The default remains CPU comparison with the existing capture path. GPU comparison
is selected explicitly with `--comparison gpu`. Shared capture is selected with
`--capture shared` and requires the experimental EVDI ABI described below.

## GPU comparison

The comparison shader reads the previous and current immutable native capture
slots. One workgroup compares each 64×16 strip and returns one changed/not-changed
word. Logical RGB includes the corresponding immutable cursor snapshot; unused X
bytes, stride padding and invisible codec rows retain the CPU comparator's
semantics. Desktop pixels are neither converted to RGB888 nor uploaded into a
second encoder input allocation.

The CPU applies the returned flags to the existing revision tracker. The tracker
continues to describe the last consumed logical input, while scanout separately
tracks successful delivery, retransmission debt and cached encoded bodies. A
failed comparison does not advance revisions. Missing baselines and resets still
establish a full keyframe.

Cursor-only updates retain the CPU fast path: their background allocation has not
changed, so comparison only needs the old and new cursor footprints. They submit
no GPU comparison work. Both native leases remain alive until comparison finishes;
the previous lease is released before encoding and USB transmission.

## Shared framebuffer capture

The existing direct path eliminates a userspace snapshot and RGB conversion, but
EVDI still copies the desktop into a mapped capture slot. Shared capture instead
leases the compositor's framebuffer and imports its DMA-buf into Vulkan. A GPU
buffer transfer copies the visible rows into the same four-slot capture pool.

This still transfers desktop pixels. It moves that transfer from the CPU to the
GPU and preserves an owned, immutable destination for comparison and encoding.
There is no CPU desktop copy or RGB conversion in the shared copier. Cursor
texture uploads remain separate and are counted separately.

The copier has its own command pool and fence on the capture thread, sharing the
encoder's Vulkan device and queue. A queue mutex serializes submissions; waiting
for a fence does not hold that mutex. A destination writer lease excludes CPU and
GPU readers until the copy completes. Transfer barriers publish the result to
both host and shader readers. Errors drain outstanding GPU work before returning
the writer to the pool.

Imported buffers are retained in a bounded cache keyed by DMA-buf device/inode
identity and allocation size. Layout is checked on every capture, including
format, modifier, dimensions, pitch, offset and allocation bounds. Source and
destination pitches may differ. The copier transfers only visible pixel bytes,
leaving destination padding initialized to zero. Stable kernel proxy identities
allow imports to be reused across captures.

### Kernel lease and proxy design

The experimental ABI version is **1**. Acquire returns a DMA-buf descriptor, a
producer-ready sync-file descriptor, framebuffer layout and an opaque lease
token. The client waits for prior producers, performs the GPU copy, then releases
the lease. A stale or revoked lease invalidates the copied image; the capture
thread publishes only after a successful release.

The kernel proxy uses the pinned system-memory pages already held by
EVDI's static import. It retains the imported GEM object and exposes those pages
through a DMA-buf with its own reservation object. The original framebuffer keeps
the producer-exclusion fence; the proxy lets the GPU reader access the same pages
without waiting on its own exclusion fence. Prior producer fences are copied into
the proxy reservation and ready sync-file before the original READ guard is
installed, with both reservation objects locked.

Normal release checks all proxy reservation fences, including BOOKKEEP use, before
signalling the original producer guard. A one-second lease expiry invalidates the
capture and returns the capture acknowledgement; it does **not** unfreeze a source
that a GPU job can still read. Error and revocation paths evict the proxy from the
kernel cache. The proxy retains the guard until its final release, after imported
references and GPU jobs are finished or reset. A hung consumer therefore requires
the independent trial supervisor to terminate it; expiry is not a guarantee that
the compositor resumes within one second. These paths require live recovery
validation in addition to the private-memory copy checks.

That separation is necessary on AMDGPU. Its reservation synchronization considers
READ fences, and a software fence with no AMDGPU owner is waited on even for an
explicit-sync allocation. Importing the original guarded DMA-buf would therefore
make the copy wait for its own lease to end. The relevant behavior is in
[AMDGPU reservation synchronization](https://github.com/torvalds/linux/blob/v7.0/drivers/gpu/drm/amd/amdgpu/amdgpu_sync.c).
AMDGPU imports a proxy with the proxy's reservation object, while EVDI's retained
original import keeps the underlying allocation alive; see
[AMDGPU DMA-buf import](https://github.com/torvalds/linux/blob/v7.0/drivers/gpu/drm/amd/amdgpu/amdgpu_dma_buf.c).

The prototype is restricted to the existing implicit-sync AMD/Xorg desktop,
linear `DRM_FORMAT_XRGB8888`, one 1920×1080 target on head 0, and separate cursor
events. Explicit-sync producers, CPU-mapped framebuffer writers, tiled or
compressed modifiers and unrelated exporters need different synchronization and
are outside this implementation. VRAM/P2P allocations and scatter-gather entries
without ordinary system pages are rejected. The prepared kernel module is not an upstream
EVDI interface or a production driver installation.

## Private-memory results

The Radeon 780M replay compared copied capture, the older direct/hash path, CPU
native revisions and GPU native revisions. Each sample used eight warmup frames
and 32 measured frames, with three rounds and reversed path order in the middle
round. The table shows mean process CPU milliseconds per frame for the two
revision paths.

| Input | CPU comparison | GPU comparison | Replay CPU reduction |
|---|---:|---:|---:|
| Repeated identical captures | 0.871 | 0.779 | 10.6% |
| Small changing region | 0.505 | 0.090 | 82.2% |
| Scrolling | 3.238 | 3.156 | 2.5% |
| Moving colours | 1.890 | 1.781 | 5.8% |
| Full-screen noise | 4.294 | 4.374 | −1.9% |
| Cursor movement with repeated captures | 1.384 | 0.889 | 35.7% |
| Cursor movement without captures | 0.0247 | 0.0239 | Approximately unchanged |

The totals include simulated capture writes, comparison, planning, GPU encoding
and framing. They exclude EVDI kernel work, the compositor, USB transport and the
physical display. Identical captures are not a quiescent desktop with no capture
events. These are component replay results, not measured whole-driver CPU, power
or temperature reductions.

The benefit depends on the input. The CPU can stop reading a strip after finding
its first difference, which makes broadly changing images cheap to compare. GPU
comparison saves considerably more CPU when most pixels remain unchanged. It
also adds submission and waiting time: mean elapsed time increased from 0.872 to
1.168 ms for identical captures and from 5.295 to 5.589 ms for scrolling. Small
updates improved from 0.640 to 0.478 ms. These measurements support retaining an
explicit choice while the live behavior is evaluated.

All 78 samples matched framed output fingerprints, output byte counts and encoded
strip counts. The native paths recorded zero desktop upload and snapshot bytes.
The cursor-only GPU path recorded zero GPU comparison dispatches.

Raw [replay results](results/gpu-shared-capture-20261021/gpu-comparison-replay.json)
and the [summary](results/gpu-shared-capture-20261021/gpu-comparison-summary.json)
retain per-stage times and comparison dispatch counts.

### DMA-buf copy checks

The private DMA-buf test exports Vulkan allocations, imports them through the
shared copier and checks every visible byte against the source. It exercises a
padded 128×33 image with a nonzero source offset and different source/destination
pitches, plus packed 1920×1080 input. Destination row padding and invisible rows
must remain zero.

The test completed **24 copies**, reused **two imports**, compared GPU encoding
against the CPU codec twice, and checked **22 GPU comparisons** against CPU pixel
comparison after copying. Vulkan validation reported **zero errors**. This checks
the transfer-to-comparison/encoding barriers as well as copy contents.

A subsequent correctness check added a GPU producer: four distinct GPU fills
release the exported allocation to the FOREIGN queue family before the shared
copier acquires it. Repeated copies through the cached import observed every new
pattern exactly and preserved codec padding. The expanded test completed **28
copies**, including these **four GPU-produced captures**, with zero validation
errors. Its [GPU-producer report](results/gpu-shared-capture-20261021/private-dma-gpu-producer-validation.json)
checks GPU cache visibility across that ownership boundary; the producer's Vulkan
fence was already complete before each copy, so this does not test the kernel's
compositor lease or sync-file wait.

Without validation, the 1080p transfer averaged **0.062 CPU ms** and **0.532 elapsed
ms** across ten measured copies after two warmups. Timing includes cached import
lookup, command recording, submission and completion waiting. It excludes fixture
generation, byte checking, EVDI ioctls and compositor lease coordination. It is
not a matched comparison with the live EVDI CPU copy.

The [validation report](results/gpu-shared-capture-20261021/private-dma-copy-validation.json)
and [performance report](results/gpu-shared-capture-20261021/private-dma-copy-performance.json)
record the device, validation setting, case counts and timings. These tests use
private allocations and cannot establish the correctness of the compositor's
lease, proxy or recovery behavior.

## Validation and prepared build

The Rust checks passed: 83 Chimera unit tests, five native-revision integration
checks, 16 EVDI client tests, seven GPU library unit checks and two worker argument
checks. The GPU-only checks additionally matched 71 independent scalar pixel
comparisons. The Python runner/comparison/recovery suite passed all 132 tests,
including upgrading an already loaded temporary candidate and recovering to the
installed original module.

The [validation summary](results/gpu-shared-capture-20261021/validation.json)
retains the counts and the explicit limits of these checks.

The isolated kernel build is prepared under
`gpu-artifacts/shared-capture-20261021/kernel/prepared-proxy/`; its
preserved [build report](results/gpu-shared-capture-20261021/kernel-build-report.json)
records the original and candidate module identities, tool
hashes and 47 host fault-injection/lifetime checks run under ASan and UBSan.
Both original and candidate kernel modules built successfully. The prepared
candidate source version is `9F72611562082A8AFD64D95`; its SHA-256 is
`76a7b666b389553cb3bdc02dc1dd8fee9f636c8f200e1a0816ed6f7ae36a9f00`.
The original is rebuilt from
`/usr/src/evdi-1.15.1` and checked against the installed module, independently of
the older batching candidate currently loaded. Nothing is installed through DKMS,
and no module was loaded while implementing or running these private tests.

Read-only worker preflight passed for direct capture with GPU comparison. Shared
capture correctly refuses to run on the old module, before display takeover.
The new module, real compositor framebuffer and combined display path still need
live validation. No combined CPU, heat or power improvement is claimed yet.

The final worker SHA-256 is
`5c21e786a1e935227821f58d5a2301930daa724b4f5a0efd4c5e5ab962a0bd85`.
The [build manifest](results/gpu-shared-capture-20261021/build-manifest.json)
preserves binary and source fingerprints. Exact binaries remain in the local,
ignored `gpu-artifacts/gpu-shared-capture-20261021/after/` directory.
The [results provenance](results/README.md) describes the published selection and
omitted machine identifiers.

## First combined live check

The module switch signs out the desktop. It retains the installed original module
as its fallback, even when upgrading the earlier temporary batching candidate.
The module remains temporary: rebooting loads the installed module again. The
following commands require the user's terminal for sudo authentication.

1. Save work, close applications that must survive a sign-out, then run the temporary module switch
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py switch \
        --build /home/cameronball/Projects/Vino/gpu-artifacts/shared-capture-20261021/kernel/prepared-proxy
    ```
2. Log back in and **verify** the normal desktop and both external monitors return, then check the switch result
    ```bash
    python3 /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py status
    ```
    **Verify** `phase` is `complete`, `shared_capture_abi` is `1`, `copy_batch_rows` is `0`, both services are active and `loaded_evdi` matches the candidate version
3. Run the bounded combined trial, move and scroll windows on the Dell, play video and **verify** smooth movement, correct colours and a visible cursor without trails
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run \
        --encoder gpu --source desktop --capture shared --comparison gpu \
        --cursor client --fps 60 --duration 120 --validation --profile-cpu
    ```
4. Let the trial finish and **verify** both desktops return with `restored: true` and no recovery errors

The combined trial bypasses EVDI's CPU copy; omit `--evdi-copy-rows`. It temporarily
uses only the Dell alongside the laptop, with the BenQ off. Ctrl+C starts normal
trial restoration early. Vulkan validation and CPU sampling are enabled to check
correctness and locate remaining work; their overhead is included in this trial.

The trial restores DisplayLink and the desktop layout while retaining the temporary
EVDI module. To return to the installed module without rebooting, save work first
and run the following command, which also signs out:

```bash
sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
    /home/cameronball/Projects/Vino/tools/gpu/evdi-module-trial.py restore
```

If the module switch changes saved monitor positions, the same helper's
`restore-layout` action restores the saved geometry after login. The separate
systemd recovery service handles failed module loads and failed worker trials;
this remains a supervised prototype, not an unattended production installation.
