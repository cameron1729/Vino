#!/usr/bin/env python3
"""Bounds checks for the binary research tool, using synthetic containers only."""
import importlib.util
from pathlib import Path
import struct
import unittest

spec=importlib.util.spec_from_file_location("extractor",Path(__file__).with_name("extract-windows-shaders.py"))
extractor=importlib.util.module_from_spec(spec)
spec.loader.exec_module(extractor)


def container():
    # Minimal structurally valid SM5 container; not an executable shader.
    body=struct.pack("<II",0x50050,2)
    size=36+8+len(body)
    return b"DXBC"+bytes(16)+struct.pack("<IIII",1,size,1,36)+b"SHEX"+struct.pack("<I",len(body))+body


class BoundsTests(unittest.TestCase):
    def test_valid_container_after_unrelated_data(self):
        parsed=list(extractor.containers(b"padding"+container()+b"tail"))
        self.assertEqual(len(parsed),1)
        self.assertEqual(parsed[0][0],7)

    def test_truncation_never_counts_a_partial_shader(self):
        blob=container()
        for end in range(len(blob)):
            self.assertEqual(list(extractor.containers(blob[:end])),[])

    def test_chunk_offset_outside_container(self):
        blob=bytearray(container())
        struct.pack_into("<I",blob,32,len(blob)+100)
        self.assertEqual(list(extractor.containers(blob)),[])

    def test_unknown_dll_is_rejected_before_decryption(self):
        with self.assertRaisesRegex(ValueError,"unexpected DLL hash"):
            extractor.decrypted_payload(b"not a driver")


if __name__=="__main__":
    unittest.main()
