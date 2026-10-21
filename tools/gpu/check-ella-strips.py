#!/usr/bin/env python3
"""Check prototype strip dumps with the existing independent Ella grammar decoder."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("strips", type=Path)
    args = parser.parse_args()
    codec = Path(__file__).resolve().parents[1] / "codec"
    sys.path.insert(0, str(codec))
    spec = importlib.util.spec_from_file_location("ella_decode", codec / "ella_decode.py")
    decoder = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(decoder)
    count = 0
    for number, line in enumerate(args.strips.read_text().splitlines(), 1):
        if not line.strip():
            continue
        data = bytes.fromhex(line)
        if len(data) < 16 or data[:2] != b"\x01\x28":
            raise ValueError(f"strip {number}: invalid header")
        row0 = int.from_bytes(data[10:12], "little")
        row1 = int.from_bytes(data[12:14], "little")
        if not 16 <= row0 <= row1 <= len(data) or any(n % 2 for n in (row0, row1, len(data))):
            raise ValueError(f"strip {number}: invalid region boundaries")
        lasts, main = decoder.decode_main(data[:row0])
        readers = [main, decoder.decode_ac_row(data[row0:row1], lasts[:8]),
                   decoder.decode_ac_row(data[row1:], lasts[8:])]
        if not all(decoder.slack_ok(reader) for reader in readers):
            raise ValueError(f"strip {number}: nonzero trailing padding")
        count += 1
    if count == 0:
        raise ValueError("no strips checked")
    print(json.dumps({"strips": count, "passed": True}))


if __name__ == "__main__":
    main()
