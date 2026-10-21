# Preserved prototype results

These reports preserve the evidence behind the October 2026 GPU encoder work.
They are a small selection from the local, ignored `gpu-artifacts/` directory;
original artifacts remain unchanged. The [manifest](manifest.json) records each
source path, source SHA-256, published SHA-256 and any omitted JSON fields.

| Directory | Evidence | Scope |
|---|---|---|
| [GPU comparison and shared capture](gpu-shared-capture-20261021/) | Replay samples, comparison summary, private DMA-buf validation/performance, userspace build identity and isolated kernel build report | The shared compositor path has **not** been live validated |
| [Native revisions](native-revisions-20261021/) | Replay samples/summary and completed manual trial/profile summaries | Exact CPU strip revisions; manual desktop activity is not a matched performance comparison |
| [USB reuse](usb-reuse-20261021/) | Packet assembly benchmark and completed manual trial/profile summaries | Component timings and sampled CPU attribution |
| [Direct capture with EVDI batching](direct-20261021/) | Four manual trials and aggregate calculations | Observed differences with differing desktop activity |

Numerical measurements and the original scope/limitations are retained. Published
JSON omits GPU device UUIDs, transient process/thread IDs and local profiler
command arrays where present. Run identifiers, ordinary kernel/build paths,
software versions, counters and function-symbol summaries remain for provenance.
No credentials or personal desktop content were found in this selection.

This directory contains no proprietary driver binaries, extracted shaders,
compiled executables/modules, framebuffer images, raw performance recordings,
process inventories or system journals. Original binary and input hashes identify
local evidence; they do not imply those files are included in the repository.
Source hashes describe the code at measurement time and may differ from later
cleanup or portability changes. These historical reports are not evidence that
shared capture is production ready or that a particular CPU reduction guarantees
lower laptop power or temperature.
