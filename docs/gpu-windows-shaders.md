## Windows shader research

Static extraction from the supplied Windows DisplayLink archive recovered **76
distinct Shader Model 5 compute shaders**, totalling 4,998,752 bytes. All 76
containers disassemble with the pinned **dxbc-spirv** tool. This establishes
that the DLL contains substantial GPU encoding code. It does not establish which
shader Windows selects for the local Ella dock or how fast that path is.

The prototype's GLSL was implemented from the existing open-source Ella codec
and checked against that codec's output. It does not execute the Windows DLL,
translate a vendor shader into the shipped implementation, or load proprietary
shader bytecode at runtime.

### Reproduction

The input was **DisplayLink USB Graphics Software for Windows12.2 M1-INF.zip**,
containing **x64/dlidusb3.dll**, version `12.2.2204.0`.

| Input | SHA-256 |
|---|---|
| ZIP | `2441f7d2213bed3f38f8a66f2cf50d7f7f2c7a03d8d256525012dcd42ec61d00` |
| x64 DLL | `91673faee443dc0109ae7a1f3adf2805fba629041cd8a4a466fe607923ed613f` |

The extractor rejects other DLL hashes. These hashes identify the inspected
input; the tool does not perform a Windows signature verification.

Additional research dependencies are Python's **cryptography** package, Git,
a C++17 compiler, Meson 1.0 or newer and Ninja. Build the disassembler locally:

```bash
mkdir -p gpu-artifacts
git clone https://github.com/doitsujin/dxbc-spirv.git gpu-artifacts/dxbc-spirv
git -C gpu-artifacts/dxbc-spirv checkout --detach \
    64f078d685725b6bd5e1a045615705f47b6aa9de
git -C gpu-artifacts/dxbc-spirv submodule update --init --recursive
meson setup gpu-artifacts/dxbc-spirv/build gpu-artifacts/dxbc-spirv \
    -Denable_tools=true -Denable_sm3=false
ninja -C gpu-artifacts/dxbc-spirv/build
python3 tools/gpu/extract-windows-shaders.py \
    "$HOME/Downloads/DisplayLink USB Graphics Software for Windows12.2 M1-INF.zip" \
    --disassembler gpu-artifacts/dxbc-spirv/build/tools/dxbc_disasm
```

The revision is also recorded in **tools/gpu/dxbc-spirv-revision.txt**. Its
SPIRV-Headers submodule resolves to
`c8ad050fcb29e42a2f57d9f59e97488f465c436d`. The command produces the local
**gpu-artifacts/windows/inventory.json**, one DXBC file per shader and one
assembly file per shader. The inventory records offsets, hashes, declarations,
workgroup sizes and instruction counts.

The [dxbc-spirv project](https://github.com/doitsujin/dxbc-spirv) provides the
disassembler. Its assembly formatting differs from Microsoft's disassembler;
instruction numbers below refer to this pinned tool's output. The separate
SPIR-V compiler is not used by this experiment.

### Container evidence

The DLL's **.rdata** raw file range is `0x598800..0xbc5000`. The first recovered encrypted
container begins at file offset `0x5ed460`. AES-128-ECB with key
`7c01a5ce4fb3f107f1906e7380d76174` decrypts that region into data containing DXBC
containers. The first container is at `0x5ed460`, the last at `0xac76c0`.

A candidate is counted only when its header, complete container length,
non-overlapping chunk ranges and shader token length are in bounds, and its
shader declares compute model 5.0. Every extracted candidate was then accepted
by the disassembler. The bytes after each container are also checked: zero
padding followed by a length byte, including a full 16-byte pad for aligned
containers. A raw search for the DXBC signature alone is not the test.
The extractor does not claim to verify the DXBC checksum or execute the shader.

The observed workgroup distribution is:

| Local size | Containers |
|---|---:|
| 16×4×1 | 10 |
| 8×2×3 | 10 |
| 16×1×1 | 56 |

The original scan started at `0x5ee000`, inside a shader. Extending the scan to
the whole decrypted **.rdata** section found one additional complete container
at `0x5ed460`. It is named `shader_75` to preserve the existing `00`–`74`
identifiers. The previously unclassified prefix was the tail of that shader.

The complete offset/size/hash inventory, including 58 recovered host labels, is in
[Windows shader inventory](gpu-windows-shaders.csv). It contains metadata only.
The DLL, decoded blobs and assembly stay ignored. The metadata-only
[host trace](gpu-windows-host-trace.json) records the static reference evidence.

### Representative observations

Instruction counts below are static occurrences in assembly, not executed
operation counts. Loops and input-dependent branches change runtime counts.

| Shader | DLL file offset | Bytes | Workgroup | Atomic instructions | Bit reverse |
|---|---|---:|---|---:|---:|
| 00 | `0x5ef5b0` | 8,860 | 16×4×1 | 1 | 0 |
| 03 | `0x5f5ee0` | 7,692 | 16×4×1 | 0 | 0 |
| 09 | `0x601c30` | 54,828 | 16×1×1 | 0 | 0 |
| 15 | `0x6517f0` | 140,600 | 8×2×3 | 146 | 130 |
| 19 | `0x6de9c0` | 84,828 | 8×2×3 | 82 | 66 |
| 30 | `0x7d8830` | 72,200 | 16×1×1 | 1 | 29 |
| 50 | `0x92cdf0` | 71,348 | 16×1×1 | 1 | 29 |
| 74 | `0xac76c0` | 82,740 | 16×1×1 | 0 | 29 |

**Observed in shader 00:** a texture input, structured 32-bit output, three-channel
arithmetic and shared intermediate arrays. Instructions 81–94 convert loaded
RGB samples to integers and form channel differences and a weighted intensity.
Instructions 102–116 implement sums and differences across four inputs.
Subsequent operations multiply by constant-buffer entries, round, and pack
multiple fields into output words. Addressing uses a 1040-word stride.

**Inference:** this is a colour conversion, transform and quantisation stage.
The data layout is consistent with an intermediate consumed by another shader,
rather than a complete USB record. It has not been paired with a Windows
dispatch capture.

**Observed in shader 09:** a structured 32-bit input and output, including a
1040-word input stride and a different output stride. Instructions 46–62
accumulate shifted bits in a register and flush a word when a bit offset crosses
32. The shader also reads code tables from constant buffers.

**Host evidence from the follow-up:** the plain `DL3` branch in the split
provider selects the newly recovered shader 75 and shader 09. Shader 00 is used
by a different branch. The original shared-stride hypothesis that paired 00
with 09 was therefore insufficient; the host argument trace narrows the pairing.

**Observed in shader 15:** 48 local invocations arranged 8×2×3, a texture input,
structured output and 8-byte metadata output. The shader reads pixels directly,
performs colour arithmetic, uses group-shared intermediates, then emits shifted
codes with atomic OR into output words. Shader 19 has a related layout with
fewer static packing operations.

**Inference:** this is a candidate for a fused path with parallel block/plane
work. The 16×3 arrangement matches the number of blocks and planes in a strip,
but the family, input mode and host constants remain unverified.

**Observed in shader 30:** 16 local invocations, texture and structured input,
1008-word and 16-word shared arrays. Around instructions 1436–1453 it clears and
reduces values in shared memory using barriers and atomic addition. Around
1454–1471 it extracts signed fields, computes magnitude bit lengths and reverses
bits before packing. Shaders 50 and 74 have similar instruction counts.

**Inference:** the DLL contains several packing strategies, including register
word accumulation and shared reduction. A Linux prototype should measure these
architectural choices; the mere presence of an atomic instruction is not
evidence of poor performance.

### Linux host tracing

The [host tracer](../tools/gpu/trace-windows-shaders.py) uses GNU objdump and
Python's standard library alongside the extractor. It follows direct references
and local shader-registration arguments in the pinned DLL; it never loads or
executes Windows code:

```bash
python3 tools/gpu/trace-windows-shaders.py \
    "$HOME/Downloads/DisplayLink USB Graphics Software for Windows12.2 M1-INF.zip"
python3 tools/gpu/test-host-trace.py
```

The run finds **84 direct references covering all 76 shaders** and **58 named
registrations**. The DLL constructs many names on the stack using a four-byte
repeating XOR key; a zero key byte is replaced with `0x5e`. The tracer follows a
decoded name through register copies into the shader-record constructor at
VA `0x1802ed700`, including the size-pointer helper at `0x1802f66d0`. Each recovered
encrypted size agrees with the complete container plus its padding.

This is a bounded static argument trace. Unsupported explicit register definitions and
calls invalidate values, partial register writes invalidate recovered pointers,
and function boundaries clear local state. The tests exercise these cases.
It does not emulate control flow or prove that a registered variant runs on a
particular dock.

Useful recovered names include:

| Shader | Host label | Registration call VA |
|---|---|---|
| 55 | `dl3_uncompacted` | `0x1802f84a4` |
| 57 | `dl32_uncompacted` | `0x1802f861b` |
| 51 | `dl3_uncompacted_diffing` | `0x1802f87c5` |
| 36 | `dl3_uncompacted_gamma` | `0x1802f8ee0` |
| 30 | `dl3_uncompacted_diffing_gamma` | `0x1802f925f` |
| 50 | `dl3_uncompacted_headeroptimised_gamma` | `0x1802f9650` |
| 27 | `dl3_uncompacted_10bit` | `0x1802f99ff` |
| 17 | `DL3_HeaderOptimisation` | `0x1802f2180` |

The complete mappings are generated in the CSV and JSON above. Shader 55 has
a 16-invocation workgroup, a 1008-word shared array, a 16-word shared array and
a 48-entry quantisation constant buffer. Instructions 532–603 show shared
intermediate writes, barriers and reductions. Instructions 740–815 read four
coefficients from shared memory, construct codes in registers and flush output
words. Those observations support testing shared-memory staging in Vino's
packer; they do not establish an identical codec configuration or a measured
Windows speed advantage.

The split-provider function at `0x1802da080` also supplies stronger pairing
evidence. References at `0x1802db151` and `0x1802db1c8` load shader 09 and shader 75.
The branch selected when `[r14+0xa8] == 0` chooses both descriptors at
`0x1802db66e`–`0x1802db6a3`, then passes them to `0x1802d81a0` at
`0x1802db736`. The same branch creates the label `DL3` at `0x1802db453`.
The nonzero branch selects shader 00 and a header-optimised packing variant.
The meaning and runtime source of the object's selection flags remain to be
traced.

### What remains unknown

We have recovered names and local registration/pairing relationships. We have
not established which encoder Windows chooses for this Ella dock, recovered
all host constant buffers or executed a vendor shader with a complete input
resource set. Disassembly and registration names alone do not determine the
active device profile, quality settings or performance.

Further Linux-only work can trace the provider-selection flags and constant
buffer construction, then build an isolated shader replay with recovered
inputs. The existing Ella CPU encoder and independent decoder remain the
correctness references for Vino's implementation.

The first implementation experiment prompted by this work is described in
[the shared-memory follow-up](gpu-encoder-followup.md).
