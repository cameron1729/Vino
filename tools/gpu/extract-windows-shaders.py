#!/usr/bin/env python3
"""Reproduce the shader inventory for DisplayLink x64 12.2.2204.0, without executing it."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess
import zipfile

DLL_HASH = "91673faee443dc0109ae7a1f3adf2805fba629041cd8a4a466fe607923ed613f"
# The original scan began inside the first shader. Keep names 00..74 stable
# while recovering that preceding container as shader_75.
START = 0x5ED460
LEGACY_START = 0x5EE000
KEY = bytes.fromhex("7c01a5ce4fb3f107f1906e7380d76174")


def containers(data):
    """Yield structurally valid SM5 compute containers; signatures alone are insufficient."""
    position = 0
    while True:
        position = data.find(b"DXBC", position)
        if position < 0:
            return
        start = position
        position += 4
        if start + 32 > len(data):
            continue
        version, size, count = struct.unpack_from("<III", data, start + 20)
        if version != 1 or not 1 <= count <= 128 or size < 32 + count * 4 or start + size > len(data):
            continue
        offsets = struct.unpack_from("<" + "I" * count, data, start + 32)
        chunks, program, intervals = [], None, []
        valid = True
        for offset in offsets:
            if offset < 32 + count * 4 or offset + 8 > size:
                valid = False
                break
            tag = data[start + offset:start + offset + 4]
            length = struct.unpack_from("<I", data, start + offset + 4)[0]
            if offset + 8 + length > size:
                valid = False
                break
            interval = (offset, offset + 8 + length)
            if any(interval[0] < b and a < interval[1] for a, b in intervals):
                valid = False
                break
            intervals.append(interval)
            chunks.append(tag.decode("ascii", errors="replace"))
            if tag in (b"SHEX", b"SHDR") and length >= 8:
                token, words = struct.unpack_from("<II", data, start + offset + 8)
                program = {"type": token >> 16, "major": (token >> 4) & 15, "minor": token & 15}
                valid &= words * 4 == length
        if valid and program == {"type": 5, "major": 5, "minor": 0}:
            yield start, data[start:start + size], chunks


def decrypted_payload(dll):
    if hashlib.sha256(dll).hexdigest() != DLL_HASH:
        raise ValueError("unexpected DLL hash; this extraction is pinned to x64 12.2.2204.0")
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
    pe = struct.unpack_from("<I", dll, 0x3C)[0]
    if dll[pe:pe + 4] != b"PE\0\0":
        raise ValueError("invalid PE signature")
    count = struct.unpack_from("<H", dll, pe + 6)[0]
    optional = struct.unpack_from("<H", dll, pe + 20)[0]
    for index in range(count):
        p = pe + 24 + optional + index * 40
        size, offset = struct.unpack_from("<II", dll, p + 16)
        if offset <= START < offset + size <= len(dll):
            raw = dll[START:offset + size]
            raw = raw[:len(raw) // 16 * 16]
            decryptor = Cipher(algorithms.AES(KEY), modes.ECB()).decryptor()
            return decryptor.update(raw) + decryptor.finalize()
    raise ValueError("payload is not inside a valid PE section")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="official INF ZIP or its x64/dlidusb3.dll")
    parser.add_argument("--output", type=Path, default=Path("gpu-artifacts/windows"))
    parser.add_argument("--disassembler", type=Path)
    args = parser.parse_args()
    raw = args.input.read_bytes()
    if zipfile.is_zipfile(args.input):
        with zipfile.ZipFile(args.input) as archive:
            dll = archive.read("x64/dlidusb3.dll")
    else:
        dll = raw
    payload = decrypted_payload(dll)
    found = list(containers(payload))
    # Stable research identifiers: earlier documents refer to shaders 00..74.
    found.sort(key=lambda item: (START + item[0] < LEGACY_START, item[0]))
    if not found:
        raise ValueError("no valid compute shaders found")
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = {
        "input_sha256": hashlib.sha256(raw).hexdigest(),
        "dll_sha256": DLL_HASH, "dll_version": "12.2.2204.0", "architecture": "x64",
        "encryption": "AES-128-ECB", "payload_file_offset": START,
        "disassembler": str(args.disassembler) if args.disassembler else None,
        "shaders": [],
    }
    for index, (offset, blob, chunks) in enumerate(found):
        padding = 16 - len(blob) % 16
        if payload[offset + len(blob):offset + len(blob) + padding] != bytes(padding - 1) + bytes([padding]):
            raise ValueError(f"invalid encrypted-container padding at {START + offset:#x}")
        stem = f"shader_{index:02}"
        binary = args.output / (stem + ".dxbc")
        binary.write_bytes(blob)
        item = {"name": stem, "file_offset": START + offset, "size": len(blob),
                "sha256": hashlib.sha256(blob).hexdigest(), "chunks": chunks, "model": "cs_5_0"}
        if args.disassembler:
            result = subprocess.run([str(args.disassembler.resolve()), str(binary.resolve())],
                                    capture_output=True, text=True, check=True)
            assembly = result.stdout
            (args.output / (stem + ".asm")).write_text(assembly)
            item["declarations"] = [line.strip() for line in assembly.splitlines()
                                    if re.search(r"\bdcl_", line)]
            item["assembly_sha256"] = hashlib.sha256(assembly.encode()).hexdigest()
            group = re.search(r"dcl_thread_group\s+(\d+),\s*(\d+),\s*(\d+)", assembly)
            item["workgroup"] = [int(v) for v in group.groups()] if group else None
            opcodes = Counter(re.findall(r"^\s*\d+:\s+([a-zA-Z0-9_]+)", assembly, re.MULTILINE))
            item["opcode_counts"] = dict(sorted(opcodes.items()))
        manifest["shaders"].append(item)
    manifest["count"] = len(found)
    manifest["unique_sha256"] = len({s["sha256"] for s in manifest["shaders"]})
    (args.output / "inventory.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"count": manifest["count"], "unique_sha256": manifest["unique_sha256"],
                      "manifest": str(args.output / "inventory.json")}, indent=2))


if __name__ == "__main__":
    main()
