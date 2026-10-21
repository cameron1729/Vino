## Vino GPU encoder prototype

This offline experiment encodes Ella/DL-3x00 RGB888 strips using Vulkan compute
and compares the result with Vino's existing CPU codec. It includes the colour
transform, Haar transform, quantisation, variable-length coding and bit packing.
The existing CPU code still detects damage, caches strips and frames records.

The implementation, commands and measurement criteria are described in
[GPU encoder prototype](../../docs/gpu-encoder.md). See
[Windows shader research](../../docs/gpu-windows-shaders.md) for the reproducible
DLL extraction, [initial measurements](../../docs/gpu-encoder-results.md) and the
[shared-memory follow-up](../../docs/gpu-encoder-followup.md) for the hardware results.

From the repository root:

```bash
RUSTFLAGS='-C target-cpu=native' cargo build \
    --manifest-path revdi/Cargo.toml -p vino-gpu-encoder --release --locked
revdi/target/release/vino-gpu devices
revdi/target/release/vino-gpu verify --output gpu-artifacts/verification
```

The CLI never opens a USB device. It is not installed into the running
DisplayLink stack and does not change kernel modules, firmware or display
configuration. The Windows DLL is an optional research input; neither building
nor running the encoder requires it.
