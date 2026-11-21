"""Check PE32+ imports for dependencies beyond Windows 10/11 system libraries."""

import argparse
from pathlib import Path
import struct


# Keep this deliberately narrow. A new native dependency requires review, not
# a blanket allowance just because the hosted runner happens to provide it.
# BCryptPrimitives (ProcessPrng) and ComBase (Windows Runtime) are OS components:
# https://learn.microsoft.com/en-us/windows/win32/seccng/processprng
# https://learn.microsoft.com/en-us/windows/win32/api/roapi/nf-roapi-roinitialize
SYSTEM_DLLS = {
    "advapi32.dll", "bcrypt.dll", "bcryptprimitives.dll", "cfgmgr32.dll",
    "combase.dll", "crypt32.dll", "dbghelp.dll",
    "gdi32.dll", "iphlpapi.dll", "kernel32.dll", "msvcrt.dll", "ncrypt.dll",
    "netapi32.dll", "ntdll.dll", "ole32.dll", "oleaut32.dll", "powrprof.dll",
    "propsys.dll", "psapi.dll", "rpcrt4.dll", "secur32.dll", "setupapi.dll",
    "shell32.dll", "shlwapi.dll", "user32.dll", "userenv.dll", "ucrtbase.dll",
    "version.dll", "winhttp.dll", "winmm.dll", "wintrust.dll", "ws2_32.dll",
    "wtsapi32.dll", "normaliz.dll", "synchronization.dll",
}
MAX_BINARY_SIZE = 256 * 1024 * 1024


def imported_dlls(path: Path) -> set[str]:
    if path.stat().st_size > MAX_BINARY_SIZE:
        raise ValueError("Windows binary exceeds the inspection limit")
    data = path.read_bytes()

    def value(format_string: str, offset: int):
        size = struct.calcsize(format_string)
        if offset < 0 or offset + size > len(data):
            raise ValueError("truncated PE binary")
        return struct.unpack_from(format_string, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("expected a Windows executable")
    pe, = value("<I", 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("invalid PE signature")
    machine, count = value("<HH", pe + 4)
    optional_size, = value("<H", pe + 20)
    optional = pe + 24
    magic, = value("<H", optional)
    if machine != 0x8664 or magic != 0x20B or count > 96 or optional_size < 224:
        raise ValueError("expected an x64 PE32+ binary")
    image_base, = value("<Q", optional + 24)
    header_size, = value("<I", optional + 60)
    directories, = value("<I", optional + 108)
    if directories < 14:
        raise ValueError("missing PE data directories")
    sections = []
    for index in range(count):
        section = optional + optional_size + index * 40
        virtual_size, virtual_address, raw_size, raw_offset = value("<IIII", section + 8)
        if raw_offset + raw_size > len(data):
            raise ValueError("truncated PE section")
        sections.append((virtual_address, max(virtual_size, raw_size), raw_offset, raw_size))

    def offset_for(rva: int) -> int:
        if 0 < rva < min(header_size, len(data)):
            return rva
        for start, size, raw, raw_size in sections:
            if start <= rva < start + size and rva - start < raw_size:
                return raw + rva - start
        raise ValueError("PE reference outside backed sections")

    def dll_name(rva: int) -> str:
        start = offset_for(rva)
        end = data.find(b"\0", start, min(start + 256, len(data)))
        if end == -1:
            raise ValueError("unterminated PE import name")
        try:
            name = data[start:end].decode("ascii").lower()
        except UnicodeDecodeError as error:
            raise ValueError("invalid PE import name") from error
        if not name.endswith(".dll") or any(character in name for character in "/\\:"):
            raise ValueError("invalid PE DLL name")
        return name

    imports = set()
    for directory, stride, name_index in ((1, 20, 3), (13, 32, 1)):
        rva, size = value("<II", optional + 112 + directory * 8)
        if not rva:
            continue
        if size < stride or size > 1024 * 1024:
            raise ValueError("invalid PE import directory")
        terminated = False
        for index in range(min(size // stride, 4096)):
            values = value("<" + "I" * (stride // 4), offset_for(rva + index * stride))
            if not any(values):
                terminated = True
                break
            name_rva = values[name_index]
            if directory == 13 and not values[0] & 1:
                name_rva -= image_base
            imports.add(dll_name(name_rva))
        if not terminated:
            raise ValueError("unterminated PE import directory")
    if not imports:
        raise ValueError("expected Windows system imports")
    return imports


def check_binary(path: Path) -> set[str]:
    imports = imported_dlls(path)
    unexpected = sorted(
        name for name in imports
        if name not in SYSTEM_DLLS and not name.startswith(("api-ms-win-", "ext-ms-win-"))
    )
    if unexpected:
        raise ValueError("non-system DLL dependencies: " + ", ".join(unexpected))
    return imports


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="+")
    arguments = parser.parse_args()
    for path in arguments.binaries:
        try:
            imports = check_binary(path)
        except (ValueError, OSError) as error:
            parser.exit(1, f"Windows dependency check failed for {path.name}: {error}\n")
        print(f"{path.name}: system DLL imports only ({', '.join(sorted(imports))})")


if __name__ == "__main__":
    main()
