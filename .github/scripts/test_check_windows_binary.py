from pathlib import Path
import struct
import tempfile
import unittest

from check_windows_binary import check_binary, imported_dlls


def fixture(dll="kernel32.dll", delay=False):
    data = bytearray(2048)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 128)
    data[128:132] = b"PE\0\0"
    struct.pack_into("<HH", data, 132, 0x8664, 1)
    struct.pack_into("<H", data, 148, 240)
    optional = 152
    struct.pack_into("<H", data, optional, 0x20B)
    struct.pack_into("<Q", data, optional + 24, 0x140000000)
    struct.pack_into("<I", data, optional + 60, 512)
    struct.pack_into("<I", data, optional + 108, 16)
    section = optional + 240
    struct.pack_into("<IIII", data, section + 8, 1536, 0x1000, 1536, 512)
    name_rva = 0x1080
    encoded = dll.encode("ascii") + b"\0"
    data[640:640 + len(encoded)] = encoded
    if delay:
        struct.pack_into("<II", data, optional + 112 + 13 * 8, 0x1000, 64)
        struct.pack_into("<8I", data, 512, 1, name_rva, 0, 0, 0, 0, 0, 0)
    else:
        struct.pack_into("<II", data, optional + 112 + 8, 0x1000, 40)
        struct.pack_into("<5I", data, 512, 0, 0, 0, name_rva, 0)
    return data


class WindowsBinaryTests(unittest.TestCase):
    def test_accepts_windows_system_imports_and_api_sets(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.exe"
            for dll in ("KERNEL32.dll", "api-ms-win-core-synch-l1-2-0.dll", "bcrypt.dll", "ucrtbase.dll"):
                path.write_bytes(fixture(dll))
                self.assertEqual(check_binary(path), {dll.lower()})

    def test_rejects_external_crt_and_onnx_normal_and_delay_imports(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.exe"
            for dll in ("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "onnxruntime.dll"):
                for delay in (False, True):
                    path.write_bytes(fixture(dll, delay))
                    with self.subTest(dll=dll, delay=delay), self.assertRaisesRegex(ValueError, "non-system"):
                        check_binary(path)

    def test_rejects_invalid_architecture_signatures_and_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.exe"
            invalid = [b"not an executable", bytes(fixture())[:650]]
            wrong_arch = fixture()
            struct.pack_into("<H", wrong_arch, 132, 0xAA64)
            invalid.append(wrong_arch)
            bad_rva = fixture()
            struct.pack_into("<I", bad_rva, 524, 0xF0000000)
            invalid.append(bad_rva)
            for binary in invalid:
                path.write_bytes(binary)
                with self.assertRaises(ValueError):
                    imported_dlls(path)

    def test_accepts_system_delay_imports(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.exe"
            path.write_bytes(fixture("shell32.dll", delay=True))
            self.assertEqual(check_binary(path), {"shell32.dll"})


if __name__ == "__main__":
    unittest.main()
