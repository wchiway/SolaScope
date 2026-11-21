"""Verify CLI aliases and package binaries, public documentation, and a checksum."""

import argparse
import hashlib
from pathlib import Path
import re
import subprocess
import tarfile
import zipfile


DOCUMENTATION = (
    "LICENSE",
    "README.md",
    "README.zh-CN.md",
    "docs/configuration.md",
    "docs/mcp.md",
    "docs/architecture.md",
    "docs/benchmarks.md",
    "docs/implementation-status.md",
    "docs/installation.md",
    "docs/releases.md",
    "benchmarks/README.md",
    "docs/assets/solascope-overview.png",
    "docs/assets/solascope-search-flow.png",
)
WINDOWS_INSTALLERS = ("install.ps1", "install.cmd")


def build_package(root: Path, target: str) -> Path:
    if re.fullmatch(r"[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+){2,}", target) is None:
        raise ValueError("invalid Rust target")
    root = root.resolve(strict=True)
    windows = "windows" in target.split("-")
    suffix = ".exe" if windows else ""
    binaries = [
        root / "target" / target / "release" / (name + suffix)
        for name in ("solascope", "sas")
    ]
    files = [(binary, binary.name) for binary in binaries]
    public_files = DOCUMENTATION + (WINDOWS_INSTALLERS if windows else ())
    files.extend((root / name, name) for name in public_files)
    for source, name in files:
        if (
            source.is_symlink()
            or not source.is_file()
            or source.resolve(strict=True) != source
        ):
            raise FileNotFoundError(f"package input must be a regular file: {source}")
        if name in WINDOWS_INSTALLERS and source.stat().st_size > 256 * 1024:
            raise ValueError("installer resource exceeds the size limit")

    versions = [
        subprocess.check_output(
            [str(binary), "--version"], encoding="utf-8"
        ).strip()
        for binary in binaries
    ]
    if versions[0] != versions[1]:
        raise ValueError("CLI aliases return different versions")
    version = re.fullmatch(
        r"solascope ([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)?)",
        versions[0],
    )
    if version is None:
        raise ValueError("unexpected CLI version output")

    destination = root / "dist"
    destination.mkdir(exist_ok=True)
    extension = ".zip" if windows else ".tar.gz"
    archive = destination / f"solascope-{version.group(1)}-{target}{extension}"
    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for source, name in files:
                package.write(source, name)
    else:
        with tarfile.open(archive, "w:gz") as package:
            for source, name in files:
                package.add(source, arcname=name, recursive=False)

    digest = hashlib.sha256()
    with archive.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    with (destination / "SHA256SUMS").open(
        "w", encoding="utf-8", newline="\n"
    ) as checksum:
        checksum.write(f"{digest.hexdigest()}  {archive.name}\n")
    return archive


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    print(build_package(args.root, args.target).name)


if __name__ == "__main__":
    main()
