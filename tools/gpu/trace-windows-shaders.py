#!/usr/bin/env python3
"""Recover local shader registrations from the pinned Windows DLL on Linux.

This is a bounded static argument trace, not an emulator or a runtime capture.
Only a decoded stack string passed to the known shader-record constructor is
associated with a shader. Direct references are reported independently.
"""
import argparse
from bisect import bisect_right
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import struct
import subprocess
import tempfile
import zipfile

spec = importlib.util.spec_from_file_location(
    "extractor", Path(__file__).with_name("extract-windows-shaders.py"))
extractor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extractor)

BASE = 0x180000000
SHADER_RECORD = BASE + 0x2ED700
SHADER_RECORD_POINTER_SIZE = BASE + 0x2F66D0
# All 76 direct shader references fall within this pinned implementation region.
BEGIN, END = BASE + 0x2D8000, BASE + 0x30D000
VOLATILE = ("rax", "rcx", "rdx", "r8", "r9", "r10", "r11")
LABEL = re.compile(r"(?:dl3(?:2)?(?:_.*)?|DL3(?:2)?(?:_.*)?|"
                   r"SplitShaderEncoder|DefaultShaderProvider|"
                   r"UncompactedShaderEncoder|DirectComputeGpuEncoder)")
STACK = re.compile(r"\[(rbp|rsp)([+-]0x[0-9a-f]+)?\]$")
IMMEDIATE_STORE = re.compile(
    r"(BYTE|DWORD|QWORD) PTR (\[(?:rbp|rsp)(?:[+-]0x[0-9a-f]+)?\]),(0x[0-9a-f]+)$")


def stack_address(operand):
    match = STACK.fullmatch(operand)
    return (match[1], int(match[2] or "0", 16)) if match else None


def register(operand):
    aliases = {alias: "r" + suffix
               for suffix, variants in (("ax", ("eax", "ax", "al", "ah")),
                                        ("bx", ("ebx", "bx", "bl", "bh")),
                                        ("cx", ("ecx", "cx", "cl", "ch")),
                                        ("dx", ("edx", "dx", "dl", "dh")),
                                        ("si", ("esi", "si", "sil")),
                                        ("di", ("edi", "di", "dil")))
               for alias in variants}
    if operand in aliases:
        return aliases[operand]
    if re.fullmatch(r"r(?:8|9|1[0-5])[dwb]", operand):
        return operand[:-1]
    return operand if re.fullmatch(r"r(?:ax|bx|cx|dx|si|di|bp|sp|8|9|1[0-5])", operand) else None


def decode_stack(memory, pointer, address):
    """Require a fresh, complete, printable, NUL-terminated XOR string."""
    base, offset = pointer
    data = bytearray()
    for index in range(132):
        entry = memory.get((base, offset + index))
        if entry is None or not 0 <= address - entry[1] <= 1024:
            return None
        data.append(entry[0])
        if index >= 4:
            value = data[index] ^ (data[(index - 4) % 4] or 0x5E)
            if value == 0:
                decoded = bytes(b ^ (data[i % 4] or 0x5E)
                                for i, b in enumerate(data[4:-1]))
                if len(decoded) >= 3 and all(32 <= b < 127 for b in decoded):
                    return decoded.decode("ascii")
                return None
            if not 32 <= value < 127:
                return None
    return None


class Image:
    def __init__(self, data):
        if hashlib.sha256(data).hexdigest() != extractor.DLL_HASH:
            raise ValueError("unexpected DLL hash")
        self.data = data
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        optional = pe + 24
        optional_size = struct.unpack_from("<H", data, pe + 20)[0]
        self.sections = []
        self.constants = []
        for i in range(struct.unpack_from("<H", data, pe + 6)[0]):
            off = optional + optional_size + i * 40
            virtual_size, rva, size, raw = struct.unpack_from("<IIII", data, off + 8)
            self.sections.append((rva, virtual_size, raw, size))
            flags = struct.unpack_from("<I", data, off + 36)[0]
            if not flags & 0x80000000:
                self.constants.append((BASE + rva, BASE + rva + size))
        pdata_rva, pdata_size = struct.unpack_from("<II", data, optional + 112 + 3 * 8)
        pdata = self.offset(BASE + pdata_rva)
        self.functions = [struct.unpack_from("<III", data, p)[:2]
                          for p in range(pdata, pdata + pdata_size, 12)]
        self.starts = [start for start, _ in self.functions]

    def offset(self, va):
        for rva, _, raw, size in self.sections:
            if rva <= va - BASE < rva + size:
                return raw + va - BASE - rva
        raise ValueError(f"address outside file-backed PE sections: {va:#x}")

    def va(self, offset):
        for rva, _, raw, size in self.sections:
            if raw <= offset < raw + size:
                return BASE + rva + offset - raw
        raise ValueError("file offset outside PE sections")

    def function(self, address):
        i = bisect_right(self.starts, address - BASE) - 1
        if i >= 0:
            start, end = self.functions[i]
            if start <= address - BASE < end:
                return BASE + start
        return None

    def integer(self, va, size):
        if not any(start <= va and va + size <= end for start, end in self.constants):
            return None
        offset = self.offset(va)
        return int.from_bytes(self.data[offset:offset + size], "little")


def trace(lines, image, shaders):
    registers, memory, references, registrations, strings = {}, {}, [], [], []
    current_function = None
    for line in lines:
        match = re.match(r"\s*([0-9a-f]+):\s+([a-z][a-z0-9]*)\s*(.*)", line)
        if not match:
            continue
        address, op, operands = int(match[1], 16), match[2], match[3].strip()
        function = image.function(address)
        if function != current_function:
            registers.clear()
            memory.clear()
            current_function = function
        comment = re.search(r"# 0x([0-9a-f]+)", operands)
        target = int(comment[1], 16) if comment else None
        args = operands.split("#")[0].strip()
        if target in shaders:
            references.append({"instruction_va": hex(address),
                               "function_va": hex(function) if function else None,
                               "shader": shaders[target], "operation": op})
        store = IMMEDIATE_STORE.fullmatch(args)
        if op == "mov" and store:
            size = {"BYTE": 1, "DWORD": 4, "QWORD": 8}[store[1]]
            base, offset = stack_address(store[2])
            value = int(store[3], 16)
            for i in range(size):
                memory[base, offset + i] = ((value >> (8 * i)) & 255, address)
        elif op == "mov":
            unknown_store = re.match(
                r"(BYTE|WORD|DWORD|QWORD|XMMWORD) PTR (\[(?:rbp|rsp)(?:[+-]0x[0-9a-f]+)?\]),",
                args)
            if unknown_store:
                size = {"BYTE": 1, "WORD": 2, "DWORD": 4, "QWORD": 8, "XMMWORD": 16}[unknown_store[1]]
                base, offset = stack_address(unknown_store[2])
                for i in range(size):
                    memory.pop((base, offset + i), None)
        if op == "call":
            destination = int(args, 16) if re.fullmatch(r"0x[0-9a-f]+", args) else None
            pointer = registers.get("rcx")
            label = None
            if pointer and pointer[0] == "stack":
                candidate = decode_stack(memory, pointer[1], address)
                if candidate and LABEL.fullmatch(candidate):
                    label = candidate
                    strings.append({"label": label, "decode_call_va": hex(address),
                                    "decode_function_va": hex(destination) if destination else None})
            if destination in (SHADER_RECORD, SHADER_RECORD_POINTER_SIZE):
                shader, name, size = (registers.get(r) for r in ("r8", "rdx", "r9"))
                if destination == SHADER_RECORD_POINTER_SIZE:
                    size = ("integer", image.integer(size[1], 4)) if size and size[0] == "address" else None
                if shader and shader[0] == "shader" and name and name[0] == "string":
                    registrations.append({
                        "shader": shader[1], "label": name[1],
                        "decode_call_va": name[2],
                        "constructor_call_va": hex(address),
                        "constructor_va": hex(destination),
                        "function_va": hex(function) if function else None,
                        "encrypted_size": size[1] if size and size[0] == "integer" else None,
                    })
            # Calls may modify stack objects through aliases. Only strings
            # assembled again after this call can support another decode.
            memory.clear()
            for r in VOLATILE:
                registers.pop(r, None)
            if label:
                registers["rax"] = ("string", label, hex(address))
            continue
        # Follow only explicit register copies, pointer loads and integer loads.
        # Unsupported definitions invalidate the destination rather than guessing.
        operands_list = args.split(",", 1)
        dest = register(operands_list[0]) if operands_list else None
        if dest and op not in ("cmp", "test", "push"):
            value = None
            source = operands_list[1] if len(operands_list) > 1 else ""
            if op == "mov":
                value = registers.get(register(source))
                if re.fullmatch(r"0x[0-9a-f]+", source):
                    value = ("integer", int(source, 16))
                elif target is not None and source.startswith("DWORD PTR"):
                    value = ("integer", image.integer(target, 4))
                elif target is not None and source.startswith("QWORD PTR"):
                    value = ("integer", image.integer(target, 8))
            elif op == "lea":
                pointer = stack_address(source)
                if pointer:
                    value = ("stack", pointer)
                elif target in shaders:
                    value = ("shader", shaders[target])
                elif target is not None:
                    value = ("address", target)
            if operands_list[0] != dest:
                # A narrowed register cannot hold a recovered pointer/string.
                is_dword = operands_list[0].startswith("e") or operands_list[0].endswith("d")
                value = ("integer", value[1] & 0xFFFFFFFF) if (
                    is_dword and value and value[0] == "integer" and value[1] is not None
                ) else None
            registers.pop(dest, None)
            if value:
                registers[dest] = value
    return references, registrations, strings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="official INF ZIP or x64/dlidusb3.dll")
    parser.add_argument("--inventory", type=Path, default=Path("gpu-artifacts/windows/inventory.json"))
    parser.add_argument("--output", type=Path, default=Path("gpu-artifacts/windows/host-trace.json"))
    args = parser.parse_args()
    if zipfile.is_zipfile(args.input):
        with zipfile.ZipFile(args.input) as archive:
            data = archive.read("x64/dlidusb3.dll")
    else:
        data = args.input.read_bytes()
    image = Image(data)
    inventory = json.loads(args.inventory.read_text())
    if inventory["dll_sha256"] != extractor.DLL_HASH:
        raise ValueError("inventory belongs to a different DLL")
    shaders = {image.va(s["file_offset"]): s["name"] for s in inventory["shaders"]}
    with tempfile.TemporaryDirectory(prefix="vino-host-trace-") as directory:
        dll = Path(directory) / "dlidusb3.dll"
        dll.write_bytes(data)
        result = subprocess.run(
            ["objdump", "-d", "--no-show-raw-insn", "-M", "intel",
             f"--start-address={BEGIN:#x}", f"--stop-address={END:#x}", str(dll)],
            capture_output=True, text=True, check=True)
    refs, registrations, strings = trace(result.stdout.splitlines(), image, shaders)
    covered = {r["shader"] for r in refs}
    if covered != set(shaders.values()):
        raise ValueError(f"missing direct references: {set(shaders.values()) - covered}")
    sizes = {s["name"]: (s["size"] // 16 + 1) * 16 for s in inventory["shaders"]}
    for item in registrations:
        if item["encrypted_size"] != sizes[item["shader"]]:
            raise ValueError(f"shader size disagrees with host registration: {item}")
    report = {
        "dll_sha256": extractor.DLL_HASH,
        "scope": "static registrations and direct references; runtime selection is not established",
        "image_base": hex(BASE), "shader_record_constructor_va": hex(SHADER_RECORD),
        "disassembly_range": [hex(BEGIN), hex(END)],
        "inventory_sha256": hashlib.sha256(args.inventory.read_bytes()).hexdigest(),
        "tracer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "objdump_version": subprocess.run(
            ["objdump", "--version"], capture_output=True, text=True, check=True
        ).stdout.splitlines()[0],
        "references": refs, "registrations": registrations, "decoded_labels": strings,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"referenced_shaders": len(covered), "references": len(refs),
                      "named_registrations": len(registrations), "output": str(args.output)}))


if __name__ == "__main__":
    main()
