## Direct EVDI capture

The optional direct path captures native XRGB into four mapped Vulkan allocations.
The GPU reads those allocations itself, removing the full-frame RGB snapshot and
the subsequent desktop upload. The existing copied path remains the default.

EVDI still copies damaged framebuffer pixels from the kernel into the selected
allocation. CPU comparison, strip hashing, framing and USB submission also remain.
This is shared capture storage, not a DMA-BUF import of the compositor framebuffer.

### Ownership and partial updates

Each allocation holds 1920×1088 four-byte pixels; EVDI sees only the 1080 visible
rows. The pool uses about 31.9 MiB plus bounded per-row stale-span metadata.

One exclusive writer survives pending EVDI requests and late callbacks. The newest
complete slot is an immutable repair baseline, also referenced by the replaceable
mailbox. The consumer can hold its previous and replacement frames during exact
comparison, then releases the previous frame before GPU submission. Four slots
allow capture to continue while those two consumer frames remain held.

For partial damage, the producer repairs stale regions from the latest baseline,
excluding pixels supplied by the new grab. Full-screen damage requires zero repair
copy. The first image is withheld until every visible pixel has been initialized.
Frames and allocations remain alive until synchronous GPU completion, including
error cleanup. Unsupported modes and capture failures use the existing recovery.

The direct path requires separate EVDI cursor events: kernel cursor composition
can write outside the returned damage rectangles. Cursor snapshots supply the same
logical pixels to CPU hashing and shader blending. Cursor bitmap upload occurs on
shape changes; movement and hiding reuse the captured background. A software cursor
already drawn by X11 remains ordinary captured content. Missing cursor events alone
are therefore not an error; losing enabled cursor-event support is.

### Interfaces and diagnostics

- **live-trial.py** and **vino-gpu-live** accept `--capture copied|direct`
- Direct trials require GPU encoding, desktop input, client cursor handling, head 0,
  an integrated Radeon and cached/coherent host-visible memory
- `SurfaceView` carries packed RGB or a validated native surface with allocation
  identity, stride and optional immutable cursor snapshot
- The EVDI Rust interface retains owned external registrations and pins the
  pending destination; its C ABI and installed kernel module are unchanged
- Health logs report snapshot bytes, repair bytes/time, grab time, comparison time,
  peak read leases and pool exhaustion
- GPU totals report desktop pixel-upload bytes, cursor-upload bytes and direct
  dispatches; direct desktop snapshot/upload counters must remain zero

### Offline results, 21 October 2026

Three alternating pairs used eight warmup frames and 48 measured frames per path
on the Radeon 780M. These are unpaced private-memory replays, with the original
copied-path hashing loop preserved. Values are mean process CPU milliseconds per
frame across the three runs:

| Workload | Copied | Direct | Less CPU |
|---|---:|---:|---:|
| Repeated unchanged captures | 1.477 | 1.082 | 27% |
| Small updates | 1.900 | 1.394 | 27% |
| Scrolling | 5.186 | 4.048 | 22% |
| Moving colours | 3.954 | 2.741 | 31% |
| Noise | 6.053 | 5.041 | 17% |

Every paired run used less CPU with direct capture and produced the same framed
stream fingerprint, output length and encoded-strip count. Separate correctness
verification compared 114,770 strips byte for byte across 367 surfaces, plus 64
framed-head comparisons, with zero Vulkan validation errors. All measured direct
runs recorded zero RGB snapshot bytes and zero desktop pixel-upload bytes.

The capture pool, library and scanout tests cover partial damage, incomplete first
frames, pending requests, immutable leases and continued production while two
consumer frames remain held. There are 85 passing Rust tests, one ignored manual
benchmark, 92 Python trial/comparison/recovery tests and 72 independent strip
grammar checks. The direct hardware preflight passed with four mapped slots.

Raw results, verification and build hashes are in
**gpu-artifacts/direct-20261021/**. An initial replay was discarded after finding
that a generic surface wrapper had slowed the copied reference; its raw file is
explicitly named **replay-discarded-rgb-wrapper-regression.json** and is excluded
from the table.

The first physical trial, **/var/tmp/vino-live-jl99qtz5**, displayed the desktop
for about 16 seconds before the cursor library failed. It completed 814 captures
and 478 direct GPU dispatches with zero desktop pixel-upload bytes; the last
health sample also reported zero snapshot bytes and no pool exhaustion. Recovery
restored the desktop without errors. This was not a completed two-minute trial.

The failure was a premature cursor GEM handle close in the Rust EVDI library:
EVDI reuses handles for the same cursor buffer, so another queued event could
refer to the handle we had just closed. The journal recorded `MAP_DUMB` returning
`ENOENT`, then the library disabled cursor events and direct capture stopped.
Cursor handles now remain valid until the DRM session closes; every event still
copies fresh bitmap contents. Genuine mapping failures still stop direct capture.
The regression test reproduces the original failure with two queued events sharing
one handle, and passes with the fix; all nine library tests pass. Distinct cursor
buffers remain pinned until session close, matching
[upstream libevdi](https://github.com/DisplayLink/evdi/blob/v1.15.1/library/evdi_lib.c),
so long-run memory growth still needs assessment before production use.
The corrected worker completed the full two-minute desktop trial in
**/var/tmp/vino-live-qb7jqs9d**, with 7,127 captures and 4,272 direct GPU dispatches.
Desktop pixel uploads and RGB snapshot bytes remained zero, with no pool
exhaustion, cursor fallback or Vulkan validation errors. It exited successfully
and restored the original layout without recovery errors.

Neither the offline results nor these functional trials establish live driver CPU
savings, idle power, heat or production readiness.

### Testing

The offline replay includes equivalent simulated dirty capture writes, comparison,
snapshot or repair, hashing, GPU encoding and framing. It excludes real EVDI kernel
work, the compositor, USB and physical display presentation. Its unchanged-image
case repeatedly captures identical pixels and does not measure a sleeping desktop.
The fixed small rectangle does not characterize every fragmented-damage pattern.

1. Build the live worker and offline verifier with the local CPU instruction set:
    ```bash
    RUSTFLAGS='-C target-cpu=native' cargo build --manifest-path revdi/Cargo.toml \
        -p vino-gpu-encoder --features live --release --locked
    ```
2. Run native/RGB codec verification and **verify** matching output and zero Vulkan validation errors:
    ```bash
    revdi/target/release/vino-gpu --device 0 verify \
        --output gpu-artifacts/direct-verification
    ```
3. Run the private-memory replay and **verify** matching framed output, zero direct desktop snapshot/upload bytes and zero repair bytes for full-screen grabs:
    ```bash
    revdi/target/release/vino-gpu --device 0 capture-bench \
        --frames 48 --warmup 8 --rounds 3 \
        --output gpu-artifacts/direct-replay.json
    ```
4. Run the read-only preflight and **verify** that the mapped pool is available before any takeover:
    ```bash
    python3 tools/gpu/live-trial.py preflight --capture direct --cursor client \
        --fps 60 --duration 120 --validation
    ```
5. Run the supervised two-minute trial, drag and scroll on the Dell, play video and **verify** smooth movement, a correct cursor and restoration of both desktops afterward:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run \
        --encoder gpu --source desktop --capture direct --cursor client \
        --fps 60 --duration 120 --validation
    ```

Ctrl+C invokes the same restoration early. Omitting `--capture direct` selects the
copied path; the runner also remains compatible with archived workers selected
using `--binary`. No kernel/module, compositor or cron change is required.

### Combined direct capture and EVDI batching

The combined configuration also enables the existing 128-row capture-copy
optimisation. It uses the already loaded temporary EVDI candidate; the two
successful direct-only runs above did not request batching. The combined
read-only preflight and ten capture-selection/batching lifecycle tests passed.
Two subsequent combined desktop trials, **/var/tmp/vino-live-8uy33x8e** and
**/var/tmp/vino-live-0j46nkf6**, completed their full two-minute runs successfully.

| Measurement | Direct only, two runs | Direct + 128-row batching, two runs |
|---|---:|---:|
| Vino CPU, % of one core | 23.09 / 23.74 | 17.56 / 18.85 |
| Capture-thread CPU, ms per capture | 1.888 / 1.975 | 1.022 / 1.060 |
| Captures per active second | 59.39 / 59.19 | 59.18 / 60.04 |
| Mean reported AMD power, W | 18.41 / 20.33 | 15.24 / 14.62 |

Across the two runs in each configuration, observed Vino CPU averaged 22% lower.
Capture-thread CPU per capture averaged 46% lower. Process CPU includes startup;
capture CPU uses first-to-last health counter differences divided by captures in
the same interval. The capture rate is not a measurement of displayed FPS.

Both combined runs retained zero RGB snapshot bytes and zero desktop pixel
uploads, with no pool exhaustion, cursor fallback or Vulkan validation errors.
Recovery reset batching to zero without errors. An independent check confirmed
the original laptop/Dell/BenQ geometry and the running official driver.

Desktop and cursor activity differed between runs, so the total CPU reduction
is an observation rather than an isolated estimate of the batching effect.
The AMD power sensor includes other work on the device; the lower readings do
not establish Vino's energy saving or a reduction in laptop heat.
Raw events, journals, recovery records and calculations are archived in
**gpu-artifacts/direct-20261021/combined-batching/results.json** and adjacent
trial directories.
The [combined trial calculations](results/direct-20261021/combined-batching.json)
are preserved with the source; raw events and journals remain local.

1. Run the combined two-minute trial and **verify** smooth dragging, scrolling and video on the Dell with a correct cursor:
    ```bash
    sudo --preserve-env=DISPLAY,XAUTHORITY python3 \
        /home/cameronball/Projects/Vino/tools/gpu/live-trial.py run \
        --encoder gpu --source desktop --capture direct --cursor client \
        --evdi-copy-rows 128 --fps 60 --duration 120 --validation
    ```
2. Let it finish and **verify** both displays return to their original positions, with `evdi_copy_rows_restored: 0`, `restored: true` and no recovery errors

The runner enables batching after stopping DisplayLink and resets it before
restoring that service. If the laptop has rebooted into the original EVDI module,
the configuration check rejects the trial before changing the displays.
