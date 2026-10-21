## Encoder and framing improvements

This change removes redundant GPU transfers and CPU copies from Vino's existing
GPU implementation. It preserves encoded strip bytes, framed records, damage
tracking, retransmission debt and capture acknowledgement timing.

The comparison baseline is the already improved RGB-sharing/packed-coefficient
build from the preceding investigation, not the original GPU prototype or the
official DisplayLink driver. Source and binary snapshots, experiments and raw
measurements are in `gpu-artifacts/deep-20261020/`.

### What changed

- **One shader instead of two.** Transform and packing share coefficients and
  significant indices within one workgroup. This removes a 4 KiB write and read
  per strip, the coefficient/index allocations and the second dispatch
- **Direct output on integrated GPUs.** The shader writes host-visible coherent
  memory, preferring CPU-cached storage. The CPU reads only each strip's actual
  body after the barrier and fence. This removes the full-capacity staging copy;
  discrete GPUs retain the staging path
- **Clear only actual output.** After computing stream lengths, the workgroup
  clears only words within the encoded body before merging boundary bits
- **Upload only requested pixels for sparse changes.** Small batches copy their
  strip rows, merging adjacent coordinates. Batches covering at least a quarter
  of the image use the existing contiguous copy
- **Parallel stream offsets.** Workgroup scans replace serial prefix loops
- **Parallel CPU hash chains.** Four neighbouring strips advance together,
  preserving every bit of the previous hash function and using no extra threads
- **Borrow strip bodies through framing.** Removed both the planner's payload
  clones and the wrapper's temporary owned copies. The native framer accepts
  borrowed slices; its serialization logic is unchanged

The integrated output synchronization follows the coherent-memory
[Khronos compute-to-host readback example](https://docs.vulkan.org/guide/latest/synchronization_examples.html#_cpu_read_back_of_data_written_by_a_compute_shader).
The combined shader's timestamps are reported as `fused_ms`; historical
`transform_ms` and `pack_ms` fields remain zero.

### Binary research that informed the changes

The supplied Windows `dlidusb3.dll` contains fused and split encoder designs.
Shaders 15 and 55 have texture input and output buffers without a separate
coefficient-buffer input. Further host decoding found
`MinimumMemoryBandwidthForSplitShaderGB` and `SplitShaderAutoUseChecker`.
These observations motivated testing fusion against our current split design;
they do not establish which design Windows selects on this laptop.

The installed Linux `DisplayLinkManager` has a routine at
`0x4b5120..0x4b5728` that reads four-byte pixels and directly emits three
16-bit colour planes. Its scalar tail establishes the channel arithmetic.
This gives a concrete lead for removing our intermediate RGB24 surface.
Static inspection does not prove that this routine is active for the dock.

Windows shader 51 accumulates channel sums during transformation. It has no
previous-frame texture input, so this alone is not evidence of GPU dirty
skipping. Vino retains its existing hashes rather than substituting an additive
checksum.

Exact binary hashes, instruction references and disassemblies are retained in
`gpu-artifacts/deep-20261020/vendor/evidence.json` and adjacent artifacts.
No proprietary code or bytecode is included in Vino.

### Additional measured gains

Three alternating before/after pairs used two 1920×1080 surfaces at 20 frame
pairs/second, with one second of warmup and two seconds of measurement per run.
The table shows the mean of the three per-run medians for elapsed time, and
mean process CPU time per pair. The existing desktop continued running.

| Workload | Pair time before → after | Less elapsed time | CPU time before → after | Less CPU time |
|---|---:|---:|---:|---:|
| Scroll | 25.77 → 16.98 ms | 34% | 18.03 → 14.44 ms | 20% |
| Motion | 16.34 → 11.02 ms | 33% | 9.14 → 8.46 ms | 7% |
| Noise | 30.42 → 22.34 ms | 27% | 22.37 → 18.99 ms | 15% |
| Small | 4.80 → 4.33 ms | 10% | 4.90 → 4.70 ms | 4% |

GPU shader time fell a further 20–32% on full changing images and 55% on the
small-update case. Output bytes and encoded-strip counts match for every pair;
GPU dispatch counts halve. The small-update case uploads 6 KiB of pixel data
per pair instead of 11.95 MiB. Its overall CPU saving is modest because other
full-surface work remains.

The independent CPU component measurements also retained identical hashes and
framed bytes: 18–26% less planning CPU, 20–30% less CPU in the initially improved
cached-framing path, and a further 23–24% less framing CPU after removing the
native wrapper copy. These component percentages are not additive.

The bounded lookup-table packer and reusable framing scratch-buffer experiments
were rejected after performance problems; neither is in the final source.
Raw samples and binary hashes are in `comparison.json` and `implementation.json`
under the artifact directory. No whole-laptop heat or power claim is made.

### Correctness

- 16,610 exact CPU/GPU strip comparisons across 223 surfaces, including sparse
  first uploads, reordered/duplicate coordinates, dimension changes, coefficient
  limits and output overflow/recovery
- 64 framed-head comparisons and 72 independent Ella grammar checks
- Zero Vulkan validation errors
- 58 Chimera tests and five GPU package tests pass
- 36 additional original-versus-borrowed-framer cases match across Ella, Ridge
  and Navarro, both heads: 34,385,216 bytes compared
- GPU package release Clippy passes with warnings denied
- The rebuilt live worker passes its read-only Radeon/hardware preflight

This build has not yet driven the physical Dell. The desktop and installed
drivers were left in place throughout the investigation.

### Next capture improvement

Implemented as the optional [direct EVDI capture path](gpu-direct-capture.md).
The design constraints below motivated its four-slot pool and partial-update repair.

The next substantial target is an owned pool of registered XRGB buffers, allowing
EVDI to capture directly into storage subsequently read by the GPU. The library's
registration function maintains a userspace pointer table, and the grab ioctl
receives that destination pointer.

The important constraint is that EVDI only implements dirty grabs. Rotating
buffers without repairing untouched regions would leave stale pixels whenever
damage is partial. A correct pool needs immutable frame ownership until GPU
completion, enough free buffers to acknowledge capture promptly, and restoration
of untouched regions from the latest complete image. Full-screen grabs need no
such repair. Hashing/comparison must ignore the unused X byte.

Replacing RGB conversion with a full XRGB snapshot copy already measured no
improvement. Direct capture into owned GPU-readable storage removes that work;
renaming or moving the copy does not.
