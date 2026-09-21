"""Linux installers must be portable: static musl binaries with no glibc demands.

Two layers, so removing the musl target from scripts/build.py cannot pass
silently: the pure byte rule is pinned on synthetic blobs, and the actual
built artifact is parsed as ELF and must carry no PT_INTERP (no dynamic
loader) and no GLIBC_ symbol-version strings.
"""
import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import build  # noqa: E402
from harness import DIST, SUFFIX, TARGET  # noqa: E402

PT_INTERP = 3


def elf_program_header_types(data):
    if data[:4] != b"\x7fELF":
        raise AssertionError("not an ELF image")
    is64 = data[4] == 2
    little = data[5] == 1
    order = "<" if little else ">"
    if is64:
        (e_phoff,) = struct.unpack_from(order + "Q", data, 0x20)
        e_phentsize, e_phnum = struct.unpack_from(order + "HH", data, 0x36)
    else:
        (e_phoff,) = struct.unpack_from(order + "I", data, 0x1C)
        e_phentsize, e_phnum = struct.unpack_from(order + "HH", data, 0x2A)
    return [struct.unpack_from(order + "I", data, e_phoff + i * e_phentsize)[0] for i in range(e_phnum)]


class PortableLinuxRule(unittest.TestCase):
    def test_glibc_symbol_versions_are_rejected(self):
        blob = Path(self.id() + ".bin")
        blob.write_bytes(b"\x7fELF" + b"\0" * 64 + b"pidfd_spawnp@GLIBC_2.39" + b"\0" * 8)
        try:
            with self.assertRaises(ValueError):
                build.assert_portable_linux(blob)
        finally:
            blob.unlink()

    def test_glibc_dynamic_loader_is_rejected(self):
        blob = Path(self.id() + ".bin")
        blob.write_bytes(b"\x7fELF" + b"\0" * 64 + b"/lib64/ld-linux-x86-64.so.2\0")
        try:
            with self.assertRaises(ValueError):
                build.assert_portable_linux(blob)
        finally:
            blob.unlink()

    def test_clean_bytes_pass(self):
        blob = Path(self.id() + ".bin")
        blob.write_bytes(b"\x7fELF" + b"\0" * 64 + b"musl static\0")
        try:
            build.assert_portable_linux(blob)
        finally:
            blob.unlink()

    def test_linux_targets_map_to_musl_triples(self):
        self.assertEqual(build.rust_triple("linux-x64"), "x86_64-unknown-linux-musl")
        self.assertEqual(build.rust_triple("linux-arm64"), "aarch64-unknown-linux-musl")
        for other in ("darwin-arm64", "darwin-x64", "win32-x64"):
            self.assertIsNone(build.rust_triple(other))


@unittest.skipUnless(TARGET.startswith("linux-"), "Linux artifact only")
class BuiltLinuxInstaller(unittest.TestCase):
    def test_built_installer_is_static_and_glibc_free(self):
        binary = DIST / "native" / TARGET / ("raft-computer-installer" + SUFFIX)
        data = binary.read_bytes()
        self.assertNotIn(PT_INTERP, elf_program_header_types(data), "installer requests a dynamic loader")
        self.assertNotIn(b"GLIBC_", data, "installer references glibc symbol versions")
        build.assert_portable_linux(binary)
