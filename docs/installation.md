# Installing SolaScope

SolaScope's default build provides indexing, lexical search, symbol lookup, and the read-only MCP server without a model. Optional model providers require separate configuration; the installer does not enable them or install Ollama, model files, or ONNX Runtime.

## Windows: precompiled installation

The installer targets **Windows 10/11 x64**, using Windows PowerShell 5.1 already supplied by Windows. It does not require Rust/Cargo, Git, Node.js, Python, PowerShell 7, a package manager, or administrator privileges. Windows ARM64, x86, older Windows versions, and enterprise policies that prohibit script execution are not supported by this installation method.

Windows release builds enable static CRT linking, bundle SQLite, and check both executable import tables for non-system DLL dependencies. These checks are not a substitute for running the release on a clean supported machine; see [verification boundaries](implementation-status.md).

### Online installation

1. Open the repository's [published releases](https://github.com/wchiway/SolaScope/releases). Download `install.ps1` and `install.cmd` from **the same release** into a new folder.
2. Inspect the scripts before executing them. Release `SHA256SUMS` includes hashes for both standalone scripts as well as the archives; it detects corruption but is not independent publisher authentication.
3. Double-click `install.cmd`, or run the downloaded PowerShell script:

```powershell
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
```

This downloads the latest published stable release. To select a particular published version:

```powershell
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Version 0.1.1
```

Replace the example version with an available release. Drafts are not installable; an explicitly selected version may be a prerelease. Resolution is performed once, and the archive and checksum are downloaded from that same frozen release. GitHub access, HTTPS, and applicable proxy configuration are required for online installation. There is no download-and-execute pipeline.

`-ExecutionPolicy Bypass` applies only to this PowerShell process. It does not permanently change machine/user policy and does not override Group Policy, AppLocker, or Windows Defender Application Control. If organizational policy disallows the installer, use an approved deployment method rather than disabling the policy.

### Location and PATH

The default layout is:

```text
%LOCALAPPDATA%\Programs\SolaScope\
  bin\solascope.exe
  bin\sas.exe
  install.ps1
  install.cmd
  LICENSE
  README.md
  README.zh-CN.md
  docs\...
```

Only the **User** PATH is updated; unrelated entries and the Machine PATH are preserved. Open a new terminal and restart existing IDEs to pick up the change. The invoking PowerShell process can be refreshed independently, but an installer child process cannot change an existing parent CMD/terminal's environment. If another SolaScope installation has higher PATH precedence, use the new executables' absolute paths or adjust PATH explicitly.

Supported options:

| Option | Purpose |
| --- | --- |
| `-Version VERSION` | Select a published SemVer version, optionally prefixed with `v` |
| `-InstallDir PATH` | Use a new absolute local-drive directory rather than the default |
| `-NoPath` | Leave persistent and process PATH unchanged |
| `-ArchivePath FILE` | Install a local ZIP without making network requests |
| `-ChecksumPath FILE` | Use a local checksum file in offline mode |
| `-ExpectedSha256 HASH` | Use an explicitly supplied archive hash in offline mode |

Existing directories must have this installer's ownership marker and contain only installation-owned files. An unrelated or manually unpacked directory is not replaced. Install into a new directory instead. Root, system, configuration, broad user-data directories, network locations, and reparse-point paths are rejected.

### Offline installation

Transfer the Windows ZIP, its matching release `SHA256SUMS`, and the installer to a local drive. Keep the input ZIP/checksum and executing script outside the chosen installation directory.

```powershell
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 `
    -ArchivePath .\solascope-0.1.1-x86_64-pc-windows-msvc.zip `
    -ChecksumPath .\SHA256SUMS
```

Replace the filename with the downloaded version. This mode makes no HTTP requests. It checks the entire archive's SHA-256 before extraction, validates its exact public-file set and Windows-safe paths, stages both executables, and verifies their version output before replacing an existing installation. Extracted content is limited to 512 MiB. Offline checksum integrity depends on obtaining the expected hash through a trusted channel.

### Upgrade and failure recovery

Run the installer again to reinstall or upgrade. It uses a per-destination installation lock and same-volume staging/backup directories; publication failures attempt to restore the previous installation. Running MCP/watch processes must be closed before replacement. The installer never terminates existing SolaScope services.

User configuration at `%USERPROFILE%\.solascope\`, project configuration, and project indexes are not changed. If User PATH cannot be persisted, the installer reports a nonzero exit even though the binaries have been installed; use their absolute paths or rerun with `-NoPath`. A backup that cannot be cleaned up is reported with its location. Filesystem checks do not defend against a hostile process racing path changes.

`install.cmd` pauses on no-argument invocation so a double-click result remains visible. For automation, call `install.ps1` directly or set `SOLASCOPE_INSTALL_NO_PAUSE=1`; both entry points preserve failure exit status.

### Portable alternative

After verifying a ZIP's checksum, manually extract it to a new folder and run `solascope.exe` or `sas.exe` directly. This requires no PATH modification. The automated installer will not treat a manually extracted folder as an owned installation.

## Linux and macOS

Published archives contain both executables, the license, and public documentation. Verify the archive against the same release's `SHA256SUMS`, extract it, and place both executables in a directory on PATH. Linux packages currently target x86_64 GNU; macOS packages use the build runner's native architecture. Check the asset's target triple before downloading.

## Build from source

From a local checkout with the repository's supported Rust toolchain:

```bash
cargo install --path apps/solascope --locked
```

Cargo normally installs both entry points into `~/.cargo/bin`. This method requires a build environment and is independent of the precompiled Windows installer. See the [README](../README.md#install) for source builds and [release operations](releases.md) for distribution automation.
