"""Exercise the real POSIX installer with isolated release and command fixtures."""

import hashlib
import importlib.util
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

_spec = importlib.util.spec_from_file_location(
    "_solascope_package", Path(__file__).with_name("package_release.py")
)
_package = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_package)
DOCUMENTATION = _package.DOCUMENTATION
UNIX_INSTALLERS = _package.UNIX_INSTALLERS


INSTALLER = Path(__file__).resolve().parents[2] / "install.sh"
LINUX = "x86_64-unknown-linux-gnu"
MAC_INTEL = "x86_64-apple-darwin"
MAC_ARM = "aa" + "rch64-apple-darwin"


@unittest.skipIf(os.name == "nt", "POSIX installer runs on Linux/macOS")
class UnixInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="solascope-unix-tests-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.home = self.root / "home with spaces"
        self.home.mkdir()
        self.mocks = self.root / "commands"
        self.mocks.mkdir()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.work = self.root / "work"
        self.work.mkdir()
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("SOLASCOPE__") and key not in ("TAR_OPTIONS", "GZIP", "GZIP_OPT")
        }
        self.environment.update({
            "HOME": str(self.home), "USERPROFILE": str(self.home),
            "PATH": str(self.mocks) + os.pathsep + os.environ["PATH"],
            "MOCK_ASSETS": str(self.assets), "MOCK_LOG": str(self.root / "network.log"),
            "TMPDIR": str(self.root), "MOCK_OS": "Linux", "MOCK_CPU": "x86_64",
            "MOCK_LIBC": "glibc 2.39", "MOCK_BITS": "64", "MOCK_ROSETTA": "0",
        })
        self.command("uname", 'case "$1" in -s) printf "%s\\n" "$MOCK_OS";; -m) printf "%s\\n" "$MOCK_CPU";; *) exit 1;; esac')
        self.command("getconf", 'case "$1" in GNU_LIBC_VERSION) [ -n "$MOCK_LIBC" ] && printf "%s\\n" "$MOCK_LIBC";; LONG_BIT) printf "%s\\n" "$MOCK_BITS";; *) exit 1;; esac')
        self.command("sysctl", 'printf "%s\\n" "$MOCK_ROSETTA"')
        for downloader in ("curl", "wget"):
            self.command(downloader, 'printf "%s\\n" "unexpected network" >>"$MOCK_LOG"; exit 91')
        self.config = self.home / ".solascope" / ".solascope.toml"
        self.config.parent.mkdir()
        self.config.write_text("# private configuration\n")
        self.profile = self.home / ".profile"
        self.profile.write_text("# existing profile\n")

    def command(self, name, body):
        path = self.mocks / name
        path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        path.chmod(0o755)
        return path

    def package(self, version="1.2.3", target=LINUX, *, extra=None, missing=(), failure=None, version_prefix=""):
        archive = self.assets / f"solascope-{version}-{target}.tar.gz"
        with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as output:
            for name in (*DOCUMENTATION, *UNIX_INSTALLERS, "solascope", "sas"):
                if name in missing:
                    continue
                if name in ("solascope", "sas"):
                    data = f'#!/bin/sh\n{version_prefix}printf "%s\\n" "solascope {version}"\n'.encode()
                    if name == failure:
                        data += b"exit 7\n"
                else:
                    data = name.encode()
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755 if name in ("solascope", "sas") else 0o644
                output.addfile(member, io.BytesIO(data))
            if extra:
                member, data = extra
                output.addfile(member, io.BytesIO(data) if data else None)
        self.checksum(archive)
        return archive

    def checksum(self, archive):
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        path = self.assets / "SHA256SUMS"
        path.write_text(f"{digest}  {archive.name}\n")
        return path

    def run_installer(self, *arguments, success=True, shell="sh", environment=None):
        result = subprocess.run(
            [shell, str(INSTALLER), *map(str, arguments)], cwd=self.work,
            env=environment or self.environment, capture_output=True, text=True, timeout=30,
        )
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def offline(self, archive, *arguments, **kwargs):
        return self.run_installer("--archive", archive, "--checksum", self.assets / "SHA256SUMS", *arguments, **kwargs)

    def assert_preserved(self):
        self.assertEqual(self.config.read_text(), "# private configuration\n")
        self.assertEqual(self.profile.read_text(), "# existing profile\n")
        self.assertFalse((self.root / "network.log").exists())

    def mock_downloads(self, wget_only=False):
        body = '''
url= out=
while [ "$#" -gt 0 ]; do
    case "$1" in
    --output) out=$2; shift 2;;
    --output-document=*) out=${1#*=}; shift;;
    https://*) url=$1; shift;;
    *) shift;;
    esac
done
printf '%s\\n' "$url" >>"$MOCK_LOG"
case "$url" in
https://github.com/wchiway/SolaScope/releases/latest/download/SHA256SUMS) file=SHA256SUMS;;
https://github.com/wchiway/SolaScope/releases/download/v1.2.3/*) file=${url##*/};;
*) exit 92;;
esac
cp "$MOCK_ASSETS/$file" "$out"
'''
        self.command("wget" if wget_only else "curl", body)

    def test_help_and_invalid_arguments_do_not_install_or_download(self):
        self.assertIn("--bin-dir", self.run_installer("--help").stdout)
        for arguments in (("--wat",), ("--version",), ("--bin-dir=",), ("--version", "1.02.3"),
                          ("--version", "1.2.3-01"), ("--bin-dir", "relative"), ("--arch", "mips-linux"),
                          ("--sha256", "0" * 64), ("--archive", "absent"), ("--version", "1.2.3+bad+build"), ("--version", "1.2.3."),
                          ("--version", "1.2..3"), ("--version", "1.2.3-001"),
                          ("--version", "1.2.3-rc.01")):
            with self.subTest(arguments=arguments):
                self.run_installer(*arguments, success=False)
        self.assertFalse((self.home / ".local").exists())
        self.assert_preserved()

    def test_offline_install_reinstall_upgrade_and_space_paths(self):
        archive = self.package()
        destination = self.home / "custom bin 测试"
        destination.mkdir()
        unrelated = destination / "unrelated"
        unrelated.write_text("keep")
        for _ in range(2):
            self.offline(archive, "--bin-dir", destination)
        archive = self.package("1.2.4")
        self.offline(archive, "--bin-dir", destination)
        self.assertEqual(subprocess.check_output([str(destination / "solascope"), "--version"], text=True).strip(), "solascope 1.2.4")
        self.assertEqual((destination / "solascope").read_bytes(), (destination / "sas").read_bytes())
        self.assertEqual(unrelated.read_text(), "keep")
        self.assertFalse(any(path.name.startswith(".solascope-install") for path in destination.iterdir()))
        self.assert_preserved()

    def test_explicit_hash_prerelease_and_bash_compatibility(self):
        archive = self.package("1.2.3-rc.1+build.2")
        result = self.run_installer("--archive", archive, "--sha256", hashlib.sha256(archive.read_bytes()).hexdigest().upper(),
                                    "--version=v1.2.3-rc.1+build.2", shell="bash")
        self.assertIn("1.2.3-rc.1+build.2", result.stdout)
        self.assert_preserved()

    def test_checksum_corruption_missing_duplicate_and_version_mismatch(self):
        archive = self.package()
        checksum = self.assets / "SHA256SUMS"
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        for contents in (f'{"0" * 64}  {archive.name}\n', f"{digest}  missing.tar.gz\n", "not a checksum\n",
                         f"{digest}  {archive.name}\n{digest}  {archive.name}\n"):
            checksum.write_text(contents)
            self.offline(archive, success=False)
        self.checksum(archive)
        self.offline(archive, "--version", "1.2.4", success=False)
        self.offline(archive, "--sha256", "0" * 64, success=False)
        self.assertFalse((self.home / ".local").exists())
        self.assert_preserved()

    def test_rejects_malicious_entries_and_missing_binaries_before_extraction(self):
        for name in ("../escape", "/escape", ".env", "solascope", "docs/../escape"):
            member = tarfile.TarInfo(name)
            member.size = 4
            archive = self.package(extra=(member, b"evil"))
            self.offline(archive, success=False)
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.DIRTYPE, tarfile.FIFOTYPE):
            # Replace sas itself with a special entry so only name checks pass.
            member = tarfile.TarInfo("sas")
            member.type = kind
            member.linkname = "../escape"
            archive = self.package(missing=("sas",), extra=(member, None))
            self.offline(archive, success=False)
        archive = self.package(missing=("install.sh",))
        self.offline(archive, success=False)
        self.assertFalse((self.root / "escape").exists())
        self.assertFalse((self.home / ".local").exists())
        self.assert_preserved()

    def test_dot_paths_and_corrupt_compression_do_not_touch_install_directory(self):
        archive = self.package()
        for suffix in ("/./bin", "/../bin"):
            self.offline(archive, "--bin-dir", str(self.home) + suffix, success=False)
        archive.write_bytes(b"not a gzip archive")
        self.checksum(archive)
        self.offline(archive, success=False)
        self.assertFalse((self.home / ".local").exists())
        self.assert_preserved()

    def test_native_version_failure_preserves_both_installed_files(self):
        archive = self.package()
        self.offline(archive)
        destination = self.home / ".local/bin"
        old = {name: (destination / name).read_bytes() for name in ("sas", "solascope")}
        archive = self.package("1.2.4", failure="sas")
        self.offline(archive, success=False)
        for name, contents in old.items():
            self.assertEqual((destination / name).read_bytes(), contents)
        self.assert_preserved()

    def test_hanging_version_times_out_and_preserves_existing_installation(self):
        archive = self.package()
        self.offline(archive)
        destination = self.home / ".local/bin"
        before = (destination / "solascope").read_bytes()
        # Shorten only the watchdog fixture; a looping native fixture never uses sleep.
        self.command("sleep", "exit 0")
        archive = self.assets / f"solascope-1.2.4-{LINUX}.tar.gz"
        with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as output:
            for name in (*DOCUMENTATION, *UNIX_INSTALLERS, "solascope", "sas"):
                data = b"#!/bin/sh\nwhile :; do :; done\n" if name in ("solascope", "sas") else name.encode()
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755 if name in ("solascope", "sas") else 0o644
                output.addfile(member, io.BytesIO(data))
        self.checksum(archive)
        result = self.offline(archive, success=False)
        self.assertIn("timed out", result.stderr)
        self.assertEqual((destination / "solascope").read_bytes(), before)
        self.assert_preserved()

    def test_successful_version_checks_reap_watchdog_sleepers(self):
        self.environment["REAL_SLEEP"] = shutil.which("sleep")
        self.environment["MOCK_TIMERS"] = str(self.root / "timers")
        self.command("sleep", 'printf "%s\\n" "$$" >>"$MOCK_TIMERS"; exec "$REAL_SLEEP" "$@"')
        self.offline(self.package(version_prefix='"$REAL_SLEEP" 0.05\n'))
        timers = (self.root / "timers").read_text().splitlines()
        self.assertEqual(len(timers), 2)
        for timer in timers:
            with self.assertRaises(ProcessLookupError, msg=f"watchdog sleep {timer} was not reaped"):
                os.kill(int(timer), 0)
        self.assert_preserved()

    def test_multiline_version_output_preserves_existing_installation(self):
        self.offline(self.package())
        destination = self.home / ".local/bin"
        before = {name: (destination / name).read_bytes() for name in ("solascope", "sas")}
        archive = self.package("1.2.4")
        with tarfile.open(archive, "r:gz") as source:
            entries = [(member, source.extractfile(member).read()) for member in source]
        with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as output:
            for member, data in entries:
                if member.name == "sas":
                    data += b'printf "extra line\\n"\n'
                    member.size = len(data)
                output.addfile(member, io.BytesIO(data))
        self.checksum(archive)
        self.offline(archive, success=False)
        for name, contents in before.items():
            self.assertEqual((destination / name).read_bytes(), contents)
        self.assert_preserved()

    def test_lock_symlink_and_nonregular_destinations_are_preserved(self):
        archive = self.package()
        destination = self.home / ".local/bin"
        destination.mkdir(parents=True)
        (destination / ".solascope-install.lock").mkdir()
        self.offline(archive, success=False)
        self.assertTrue((destination / ".solascope-install.lock").is_dir())
        (destination / ".solascope-install.lock").rmdir()
        target = self.root / "user-file"
        target.write_text("private")
        (destination / "solascope").symlink_to(target)
        self.offline(archive, success=False)
        self.assertEqual(target.read_text(), "private")
        (destination / "solascope").unlink()
        (destination / "solascope").mkdir()
        self.offline(archive, success=False)
        link = self.home / "linked-bin"
        link.symlink_to(destination, target_is_directory=True)
        self.offline(archive, "--bin-dir", link, success=False)
        self.assert_preserved()

    def test_second_replacement_failure_rolls_back_both_binaries(self):
        archive = self.package()
        self.offline(archive)
        destination = self.home / ".local/bin"
        before = {name: (destination / name).read_bytes() for name in ("solascope", "sas")}
        real_mv = shutil.which("mv")
        self.environment["REAL_MV"] = real_mv
        self.command("mv", '''
case "$2" in */.solascope-install.*/sas) exit 71;; esac
exec "$REAL_MV" "$@"
''')
        archive = self.package("1.2.4")
        self.offline(archive, success=False)
        for name, contents in before.items():
            self.assertEqual((destination / name).read_bytes(), contents)
        self.assert_preserved()

    def test_latest_download_is_resolved_once_without_json_or_extra_tools(self):
        archive = self.package()
        self.mock_downloads()
        result = self.run_installer()
        self.assertIn("1.2.3", result.stdout)
        self.assertEqual((self.root / "network.log").read_text().splitlines(), [
            "https://github.com/wchiway/SolaScope/releases/latest/download/SHA256SUMS",
            f"https://github.com/wchiway/SolaScope/releases/download/v1.2.3/{archive.name}",
        ])

    def test_wget_fallback_and_macos_shasum_without_curl_or_sha256sum(self):
        archive = self.package()
        self.mock_downloads(wget_only=True)
        # A minimal command PATH makes command -v observe a missing curl and
        # sha256sum without installing/uninstalling anything on the machine.
        for name in ("mktemp", "mkdir", "rmdir", "dirname", "cp", "mv", "rm", "chmod", "tar", "gzip", "sort", "cmp", "tr", "sleep", "shasum", "perl"):
            executable = shutil.which(name)
            if executable:
                (self.mocks / name).symlink_to(executable)
        (self.mocks / "curl").unlink()
        environment = self.environment | {"PATH": str(self.mocks)}
        self.run_installer(environment=environment, shell=shutil.which("sh"))
        self.assertEqual((self.root / "network.log").read_text().splitlines()[-1],
                         f"https://github.com/wchiway/SolaScope/releases/download/v1.2.3/{archive.name}")

    def test_detects_macos_intel_arm_and_rosetta_without_running_foreign_code(self):
        for cpu, rosetta, target in (("x86_64", "0", MAC_INTEL), ("arm64", "0", MAC_ARM), ("x86_64", "1", MAC_ARM)):
            with self.subTest(cpu=cpu, rosetta=rosetta):
                self.environment.update(MOCK_OS="Darwin", MOCK_CPU=cpu, MOCK_ROSETTA=rosetta)
                self.package(target=target)
                self.mock_downloads()
                self.run_installer()
                self.assertIn(target, (self.root / "network.log").read_text().splitlines()[-1])

    def test_rejects_musl_32bit_linux_unsupported_os_and_missing_platform_asset(self):
        for overrides in ({"MOCK_LIBC": ""}, {"MOCK_BITS": "32"}, {"MOCK_CPU": "arm64"}, {"MOCK_OS": "FreeBSD"}):
            environment = self.environment | overrides
            self.run_installer(success=False, environment=environment)
        self.assert_preserved()
        self.package()
        self.environment.update(MOCK_OS="Darwin", MOCK_CPU="arm64")
        self.mock_downloads()
        result = self.run_installer(success=False)
        self.assertIn("no published archive checksum", result.stderr)
        self.assertEqual(len((self.root / "network.log").read_text().splitlines()), 1)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--real-archive":
        archive = Path(sys.argv[2]).resolve()
        checksum = Path(sys.argv[3]).resolve()
        with tempfile.TemporaryDirectory(prefix="solascope-real-unix-install-") as directory:
            root = Path(directory).resolve()
            destination = root / "bin with spaces"
            environment = {key: value for key, value in os.environ.items() if not key.startswith("SOLASCOPE__")}
            environment.update(HOME=str(root), USERPROFILE=str(root))
            config = root / ".solascope" / ".solascope.toml"
            config.parent.mkdir()
            config.write_text("# preserve configuration\n")
            profile = root / ".profile"
            profile.write_text("# preserve shell profile\n")
            destination.mkdir()
            unrelated = destination / "unrelated"
            unrelated.write_text("preserve unrelated file\n")
            for _ in range(2):
                subprocess.run(
                    ["sh", str(INSTALLER), "--archive", str(archive), "--checksum", str(checksum), "--bin-dir", str(destination)],
                    env=environment, cwd=INSTALLER.parent, check=True, timeout=60,
                )
                versions = [subprocess.check_output(
                    [str(destination / name), "--version"], env=environment,
                    cwd=INSTALLER.parent, text=True, timeout=30,
                ).strip() for name in ("solascope", "sas")]
                if versions[0] != versions[1] or not versions[0].startswith("solascope "):
                    raise AssertionError(f"installed CLI versions do not match: {versions}")
                if not archive.name.startswith("solascope-" + versions[0].split()[1] + "-"):
                    raise AssertionError("installed version differs from the archive filename")
                if config.read_text() != "# preserve configuration\n" or profile.read_text() != "# preserve shell profile\n":
                    raise AssertionError("installation changed existing configuration")
                if unrelated.read_text() != "preserve unrelated file\n":
                    raise AssertionError("installation changed an unrelated binary-directory file")
            print("Real Unix release archive installed and reinstalled; both CLI versions and existing configuration verified.")
    else:
        unittest.main()
