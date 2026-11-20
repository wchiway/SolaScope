import hashlib
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from package_release import DOCUMENTATION, build_package


class PackageReleaseTests(unittest.TestCase):
    def fixture(self, root: Path, target: str) -> set[str]:
        suffix = ".exe" if "windows" in target.split("-") else ""
        for name in ("solascope", "sas"):
            binary = root / "target" / target / "release" / (name + suffix)
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_bytes(b"test executable\n")
            binary.chmod(0o755)
        for name in DOCUMENTATION:
            source = root / name
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text(name, encoding="utf-8")
        (root / ".env").write_text("private fixture", encoding="utf-8")
        (root / ".solascope").mkdir()
        (root / ".solascope" / "index.db").write_bytes(b"private index")
        return set(DOCUMENTATION) | {"solascope" + suffix, "sas" + suffix}

    def assert_checksum(self, archive: Path) -> None:
        expected = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual(
            (archive.parent / "SHA256SUMS").read_bytes(),
            f"{expected}  {archive.name}\n".encode("utf-8"),
        )

    @patch("package_release.subprocess.check_output", return_value="solascope 0.1.0\n")
    def test_unix_packages_include_aliases_docs_and_checksum(self, version) -> None:
        target = "x86_64-unknown-linux-gnu"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = self.fixture(root, target)
            archive = build_package(root, target)
            self.assertEqual(archive.name, f"solascope-0.1.0-{target}.tar.gz")
            with tarfile.open(archive, "r:gz") as package:
                self.assertEqual(set(package.getnames()), expected)
                for name in DOCUMENTATION:
                    self.assertEqual(package.extractfile(name).read(), name.encode())
                if os.name != "nt":
                    for name in ("solascope", "sas"):
                        self.assertEqual(package.getmember(name).mode & 0o111, 0o111)
            self.assert_checksum(archive)
        self.assertEqual(version.call_count, 2)

    @patch("package_release.subprocess.check_output", return_value="solascope 0.1.0\n")
    def test_windows_packages_include_executable_aliases_docs_and_checksum(self, version) -> None:
        target = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = self.fixture(root, target)
            archive = build_package(root, target)
            self.assertEqual(archive.name, f"solascope-0.1.0-{target}.zip")
            with zipfile.ZipFile(archive) as package:
                self.assertEqual(set(package.namelist()), expected)
                for name in DOCUMENTATION:
                    self.assertEqual(package.read(name), name.encode())
            self.assert_checksum(archive)
        self.assertEqual(version.call_count, 2)

    @patch(
        "package_release.subprocess.check_output",
        side_effect=["solascope 0.1.0\n", "solascope 0.2.0\n"],
    )
    def test_mismatched_alias_versions_do_not_create_a_package(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, "x86_64-unknown-linux-gnu")
            with self.assertRaisesRegex(ValueError, "different versions"):
                build_package(root, "x86_64-unknown-linux-gnu")
            self.assertFalse((root / "dist").exists())

    @patch("package_release.subprocess.check_output", return_value="solascope ../escape\n")
    def test_invalid_version_does_not_create_a_package(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, "x86_64-unknown-linux-gnu")
            with self.assertRaisesRegex(ValueError, "unexpected CLI version"):
                build_package(root, "x86_64-unknown-linux-gnu")
            self.assertFalse((root / "dist").exists())

    @patch("package_release.subprocess.check_output", return_value="solascope 0.1.0\n")
    def test_missing_documentation_does_not_create_a_package(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, "x86_64-unknown-linux-gnu")
            (root / "README.zh-CN.md").unlink()
            with self.assertRaises(FileNotFoundError):
                build_package(root, "x86_64-unknown-linux-gnu")
            self.assertFalse((root / "dist").exists())
            version.assert_not_called()

    @unittest.skipIf(os.name == "nt", "creating Windows symlinks requires privileges")
    @patch("package_release.subprocess.check_output", return_value="solascope 0.1.0\n")
    def test_symlink_documentation_cannot_include_private_data(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, "x86_64-unknown-linux-gnu")
            readme = root / "README.md"
            readme.unlink()
            readme.symlink_to(root / ".env")
            with self.assertRaises(FileNotFoundError):
                build_package(root, "x86_64-unknown-linux-gnu")
            self.assertFalse((root / "dist").exists())
            version.assert_not_called()

    @unittest.skipIf(os.name == "nt", "creating Windows symlinks requires privileges")
    @patch("package_release.subprocess.check_output", return_value="solascope 0.1.0\n")
    def test_symlink_documentation_directory_is_rejected(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root, "x86_64-unknown-linux-gnu")
            assets = root / "docs" / "assets"
            relocated = root / "relocated-assets"
            assets.rename(relocated)
            assets.symlink_to(relocated, target_is_directory=True)
            with self.assertRaises(FileNotFoundError):
                build_package(root, "x86_64-unknown-linux-gnu")
            self.assertFalse((root / "dist").exists())
            version.assert_not_called()

    @patch("package_release.subprocess.check_output")
    def test_invalid_target_cannot_escape_the_package_directory(self, version) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for target in ("../escape", "x86_64-unknown-linux-gnu/../escape"):
                with self.subTest(target=target), self.assertRaisesRegex(
                    ValueError, "invalid Rust target"
                ):
                    build_package(root, target)
            self.assertFalse((root / "dist").exists())
            version.assert_not_called()


if __name__ == "__main__":
    unittest.main()
