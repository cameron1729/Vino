#!/usr/bin/env python3
"""Synthetic evidence checks: never associate stale or clobbered host arguments."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "tracer", Path(__file__).with_name("trace-windows-shaders.py"))
tracer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tracer)


class Image:
    def function(self, _address):
        return 0x180100000

    def integer(self, address, size):
        return 256 if (address, size) == (0x180600100, 4) else None


def program(middle=(), tail=()):
    key = b"AbCd"
    name = b"dl3_uncompacted\0"
    encoded = key + bytes(value ^ key[i % 4] for i, value in enumerate(name))
    body = [f"mov BYTE PTR [rbp+0x{0x80 + i:x}],0x{value:x}"
            for i, value in enumerate(encoded)]
    body += ["lea rcx,[rbp+0x80]", "call 0x18021b3b0", *middle,
             "lea r8,[rip+0x1] # 0x180600000",
             "mov rdx,rax", "mov r9d,0x100", *tail,
             f"call {tracer.SHADER_RECORD:#x}"]
    return [f"{0x180100000 + i * 8:x}: {line}" for i, line in enumerate(body)]


def registrations(lines):
    return tracer.trace(lines, Image(), {0x180600000: "shader_test"})[1]


class TraceTests(unittest.TestCase):
    def test_direct_arguments_recover_name_and_size(self):
        found = registrations(program())
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0]["label"], "dl3_uncompacted")
        self.assertEqual(found[0]["encrypted_size"], 256)

    def test_unknown_call_clobbers_returned_label(self):
        self.assertEqual(registrations(program(middle=["call 0x180399999"])), [])

    def test_nonvolatile_copy_preserves_label_across_allocation(self):
        found = registrations(program(middle=[
            "mov rbx,rax", "call 0x180399999", "mov rax,rbx"]))
        self.assertEqual(len(found), 1)

    def test_partial_register_write_invalidates_pointer(self):
        self.assertEqual(registrations(program(tail=["mov dl,0x1"])), [])

    def test_pointer_cannot_survive_narrowed_copy(self):
        self.assertEqual(registrations(program(tail=["mov edx,eax"])), [])

    def test_unknown_shader_is_not_registered(self):
        self.assertEqual(registrations(program(tail=[
            "lea r8,[rip+0x1] # 0x180600010"])), [])

    def test_pointer_size_helper_dereferences_constant(self):
        lines = program(tail=["lea r9,[rip+0x1] # 0x180600100"])
        lines[-1] = lines[-1].replace(
            hex(tracer.SHADER_RECORD), hex(tracer.SHADER_RECORD_POINTER_SIZE))
        self.assertEqual(registrations(lines)[0]["encrypted_size"], 256)

    def test_incomplete_or_old_stack_string_is_rejected(self):
        memory = {("rbp", i): (value, 0) for i, value in enumerate(b"AbCd" + b"?" * 8)}
        self.assertIsNone(tracer.decode_stack(memory, ("rbp", 0), 0))
        self.assertIsNone(tracer.decode_stack(memory, ("rbp", 0), 1025))

    def test_unknown_stack_write_invalidates_encoded_string(self):
        lines = program()
        index = next(i for i, line in enumerate(lines) if "lea rcx," in line)
        address = int(lines[index].split(":")[0], 16)
        lines.insert(index, f"{address - 1:x}: mov DWORD PTR [rbp+0x84],eax")
        self.assertEqual(registrations(lines), [])

    def test_decode_cannot_reuse_stack_bytes_across_calls(self):
        self.assertEqual(registrations(program(middle=[
            "lea rcx,[rbp+0x80]", "call 0x18021b3b0"])), [])

    def test_wrong_dll_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unexpected DLL hash"):
            tracer.Image(b"not the pinned driver")


if __name__ == "__main__":
    unittest.main()
