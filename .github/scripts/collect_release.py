"""Collect verified native CI packages into a new GitHub Release asset set."""

import argparse
import hashlib
import importlib.util
from pathlib import Path
import re
import shutil
import stat
import tarfile
import zipfile


_spec = importlib.util.spec_from_file_location(
    "_solascope_package", Path(__file__).with_name("package_release.py")
)
_package = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_package)
DOCUMENTATION = _package.DOCUMENTATION
WINDOWS_INSTALLERS = _package.WINDOWS_INSTALLERS
MAX_UNPACKED_SIZE = 512 * 1024 * 1024


def validate_version(version: str) -> None:
    if re.fullmatch(
        r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?",
        version,
    ) is None:
        raise ValueError("invalid release version")


def platform_for(target: str) -> str:
    if re.fullmatch(r"[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+){2,}", target) is None:
        raise ValueError("invalid package target")
    if "windows" in target.split("-"):
        return "windows"
    if target.endswith("-apple-darwin"):
        return "macos"
    if "linux" in target.split("-"):
        return "linux"
    raise ValueError("unsupported native package platform")


def verify_archive(archive: Path, windows: bool) -> None:
    suffix = ".exe" if windows else ""
    expected = set(DOCUMENTATION) | {"solascope" + suffix, "sas" + suffix}
    if windows:
        expected.update(WINDOWS_INSTALLERS)
    names = set()
    size = 0
    if windows:
        with zipfile.ZipFile(archive) as package:
            for member in package.infolist():
                kind = stat.S_IFMT(member.external_attr >> 16)
                if member.is_dir() or kind not in (0, stat.S_IFREG):
                    raise ValueError("package contains a non-regular entry")
                if member.filename not in expected or member.filename in names:
                    raise ValueError("unexpected or duplicate package entry")
                names.add(member.filename)
                if member.filename in WINDOWS_INSTALLERS and member.file_size > 256 * 1024:
                    raise ValueError("installer resource exceeds the size limit")
                size += member.file_size
                if size > MAX_UNPACKED_SIZE:
                    raise ValueError("package exceeds unpacked size limit")
            if package.testzip() is not None:
                raise ValueError("invalid ZIP package contents")
    else:
        with tarfile.open(archive, "r:gz") as package:
            for member in package:
                if not member.isfile():
                    raise ValueError("package contains a non-regular entry")
                if member.name not in expected or member.name in names:
                    raise ValueError("unexpected or duplicate package entry")
                names.add(member.name)
                size += member.size
                if size > MAX_UNPACKED_SIZE:
                    raise ValueError("package exceeds unpacked size limit")
    if names != expected:
        raise ValueError("package is missing binaries or documentation")


def collect_releases(artifacts: Path, output: Path, version: str) -> list[Path]:
    validate_version(version)
    artifacts = artifacts.resolve(strict=True)
    packages = []
    platforms = set()
    for directory in sorted(artifacts.iterdir()):
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError("artifact must be a regular directory")
        if not directory.name.startswith("solascope-"):
            raise ValueError("unexpected artifact name")
        target = directory.name.removeprefix("solascope-")
        platform = platform_for(target)
        if platform in platforms:
            raise ValueError("duplicate native platform package")
        platforms.add(platform)
        extension = ".zip" if platform == "windows" else ".tar.gz"
        archive = directory / f"solascope-{version}-{target}{extension}"
        checksum = directory / "SHA256SUMS"
        if {entry.name for entry in directory.iterdir()} != {archive.name, checksum.name}:
            raise ValueError("artifact files do not match the release version and target")
        for source in (archive, checksum):
            if source.is_symlink() or not source.is_file() or source.resolve() != source:
                raise ValueError("artifact input must be a regular file")
        lines = checksum.read_text(encoding="utf-8").splitlines()
        match = re.fullmatch(r"([0-9a-fA-F]{64})  ([^\r\n]+)", lines[0]) if len(lines) == 1 else None
        if match is None or match.group(2) != archive.name:
            raise ValueError("invalid artifact checksum entry")
        digest = hashlib.sha256()
        with archive.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != match.group(1).lower():
            raise ValueError("artifact checksum mismatch")
        verify_archive(archive, platform == "windows")
        packages.append((archive, digest.hexdigest()))
    if platforms != {"linux", "macos", "windows"}:
        raise ValueError("release requires all three native platform packages")

    # Export only the two validated installer resources, never execute archives.
    windows_archive = next(archive for archive, _ in packages if archive.suffix == ".zip")
    with zipfile.ZipFile(windows_archive) as package:
        installers = {name: package.read(name) for name in WINDOWS_INSTALLERS}
    output.mkdir(parents=True, exist_ok=False)
    result = []
    checksums = []
    for archive, digest in packages:
        destination = output / archive.name
        shutil.copy2(archive, destination)
        result.append(destination)
        checksums.append((destination.name, digest))
    for name, contents in installers.items():
        (output / name).write_bytes(contents)
        checksums.append((name, hashlib.sha256(contents).hexdigest()))
    with (output / "SHA256SUMS").open("w", encoding="utf-8", newline="\n") as checksum:
        for name, digest in sorted(checksums):
            checksum.write(f"{digest}  {name}\n")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("dist/release"))
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    packages = collect_releases(args.artifacts, args.output, args.version)
    print(f"Prepared {len(packages)} native packages for v{args.version}")


if __name__ == "__main__":
    main()
