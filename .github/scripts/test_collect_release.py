import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from collect_release import collect_releases, validate_version
from package_release import DOCUMENTATION, build_package


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


class CollectReleaseTests(unittest.TestCase):
    def fixture(self, root: Path, version: str = "0.1.0") -> Path:
        artifacts = root / "artifacts"
        artifacts.mkdir()
        with patch(
            "package_release.subprocess.check_output",
            return_value=f"solascope {version}\n",
        ):
            for target in TARGETS:
                project = root / "projects" / target
                suffix = ".exe" if "windows" in target else ""
                for name in ("solascope", "sas"):
                    binary = project / "target" / target / "release" / (name + suffix)
                    binary.parent.mkdir(parents=True, exist_ok=True)
                    binary.write_bytes(b"test executable\n")
                    binary.chmod(0o755)
                for name in DOCUMENTATION:
                    source = project / name
                    source.parent.mkdir(parents=True, exist_ok=True)
                    source.write_text(name, encoding="utf-8")
                archive = build_package(project, target)
                directory = artifacts / ("solascope-" + target)
                directory.mkdir()
                shutil.copy2(archive, directory / archive.name)
                shutil.copy2(archive.parent / "SHA256SUMS", directory / "SHA256SUMS")
        return artifacts

    def replace_checksum(self, archive: Path) -> None:
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (archive.parent / "SHA256SUMS").write_text(
            f"{digest}  {archive.name}\n", encoding="utf-8"
        )

    def test_collects_three_packages_with_one_complete_checksum_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            output = root / "release"
            packages = collect_releases(artifacts, output, "0.1.0")
            self.assertEqual(len(packages), 3)
            self.assertEqual(len(list(output.iterdir())), 4)
            expected = "".join(
                f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
                for path in sorted(packages, key=lambda path: path.name)
            )
            self.assertEqual((output / "SHA256SUMS").read_bytes(), expected.encode())

    def test_standalone_cli_works_with_isolated_python(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root, "0.2.0-beta.1+build.2")
            script = Path(__file__).with_name("collect_release.py")
            result = subprocess.run(
                [
                    sys.executable, "-I", "-B", str(script),
                    "--artifacts", str(artifacts), "--output", str(root / "release"),
                    "--version", "0.2.0-beta.1+build.2",
                ],
                capture_output=True,
                encoding="utf-8",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Prepared 3 native packages", result.stdout)

    def test_rejects_a_missing_platform_before_creating_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            shutil.rmtree(artifacts / ("solascope-" + TARGETS[0]))
            with self.assertRaisesRegex(ValueError, "all three"):
                collect_releases(artifacts, root / "release", "0.1.0")
            self.assertFalse((root / "release").exists())

    def test_rejects_the_wrong_source_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            with self.assertRaisesRegex(ValueError, "release version"):
                collect_releases(artifacts, root / "release", "0.2.0")
            self.assertFalse((root / "release").exists())

    def test_rejects_a_modified_archive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            archive = next((artifacts / ("solascope-" + TARGETS[0])).glob("*.tar.gz"))
            with archive.open("ab") as output:
                output.write(b"modified archive")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                collect_releases(artifacts, root / "release", "0.1.0")
            self.assertFalse((root / "release").exists())

    def test_rejects_a_checksum_that_names_another_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            checksum = artifacts / ("solascope-" + TARGETS[0]) / "SHA256SUMS"
            checksum.write_text("0" * 64 + "  ../escape.tar.gz\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "checksum entry"):
                collect_releases(artifacts, root / "release", "0.1.0")
            self.assertFalse((root / "release").exists())

    def test_rejects_private_or_traversing_tar_entries(self) -> None:
        for name in (".env", "../escape", "solascope"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                artifacts = self.fixture(root)
                archive = next((artifacts / ("solascope-" + TARGETS[0])).glob("*.tar.gz"))
                with tarfile.open(archive, "r:gz") as source:
                    members = source.getmembers()
                    contents = {member.name: source.extractfile(member).read() for member in members}
                with tarfile.open(archive, "w:gz") as output:
                    import io
                    for member in members:
                        output.addfile(member, io.BytesIO(contents[member.name]))
                    output.addfile(tarfile.TarInfo(name))
                self.replace_checksum(archive)
                with self.assertRaisesRegex(ValueError, "unexpected or duplicate"):
                    collect_releases(artifacts, root / "release", "0.1.0")
                self.assertFalse((root / "release").exists())

    def test_rejects_zip_symlink_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            archive = next((artifacts / ("solascope-" + TARGETS[2])).glob("*.zip"))
            with zipfile.ZipFile(archive, "w") as output:
                member = zipfile.ZipInfo("solascope.exe")
                member.create_system = 3
                member.external_attr = 0o120777 << 16
                output.writestr(member, "../private")
            self.replace_checksum(archive)
            with self.assertRaisesRegex(ValueError, "non-regular"):
                collect_releases(artifacts, root / "release", "0.1.0")
            self.assertFalse((root / "release").exists())

    @unittest.skipIf(os.name == "nt", "creating Windows symlinks requires privileges")
    def test_rejects_symlink_artifact_directories(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            source = artifacts / ("solascope-" + TARGETS[0])
            relocated = root / "relocated"
            source.rename(relocated)
            source.symlink_to(relocated, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "regular directory"):
                collect_releases(artifacts, root / "release", "0.1.0")
            self.assertFalse((root / "release").exists())

    def test_does_not_overwrite_existing_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = self.fixture(root)
            output = root / "release"
            output.mkdir()
            marker = output / "keep.txt"
            marker.write_text("keep", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                collect_releases(artifacts, output, "0.1.0")
            self.assertEqual(marker.read_text(encoding="utf-8"), "keep")
            self.assertEqual(list(output.iterdir()), [marker])

    def test_rejects_invalid_version_strings(self) -> None:
        for version in ("../escape", "v0.1.0", "0.1.0\ntag=other", ""):
            with self.subTest(version=version), self.assertRaises(ValueError):
                validate_version(version)


if __name__ == "__main__":
    unittest.main()
