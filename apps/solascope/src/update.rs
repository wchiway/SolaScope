//! Native, explicit self-update. No project configuration or model adapter is loaded.
mod download;

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use fs2::FileExt;
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};
use solascope_core::{Error, Result};
use tempfile::TempDir;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize)]
pub(super) struct Report {
    status: &'static str,
    current_version: String,
    latest_version: String,
    target: String,
    release_url: String,
    directory: Option<PathBuf>,
    warnings: Vec<String>,
}

fn failure(message: impl Into<String>) -> Error {
    Error::Io(io::Error::other(message.into()))
}

#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Check the latest stable release without changing the installation.
    #[arg(long, conflicts_with_all = ["archive", "force"])]
    check: bool,
    /// Reinstall the same version (a downgrade is never performed).
    #[arg(long)]
    force: bool,
    /// Use a local release archive instead of contacting GitHub.
    #[arg(long, requires = "checksum")]
    archive: Option<PathBuf>,
    /// SHA256SUMS matching the local release archive.
    #[arg(long, requires = "archive")]
    checksum: Option<PathBuf>,
}

pub(super) fn run(options: &Options) -> Result<Report> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let source = download::Source::github()?;
        let current = Version::parse(VERSION).map_err(|error| failure(error.to_string()))?;
        perform(&source, &current, target()?, options, None).await
    })
}

pub(super) fn print_report(report: &Report, json: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut output, report).map_err(|error| failure(error.to_string()))?;
        writeln!(output)?;
    } else {
        match report.status {
            "updated" => writeln!(
                output,
                "Updated SolaScope {} -> {} (both solascope and sas).",
                report.current_version, report.latest_version
            )?,
            "update_available" => writeln!(
                output,
                "SolaScope {} is available (installed {}). Run `solascope update` or `sas update`.",
                report.latest_version, report.current_version
            )?,
            _ => writeln!(
                output,
                "SolaScope {} is current; latest stable release is {}.",
                report.current_version, report.latest_version
            )?,
        }
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }
    Ok(())
}

fn target() -> Result<&'static str> {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Ok("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(concat!("a", "arch64-apple-darwin"))
    } else if cfg!(all(
        target_os = "windows",
        target_arch = "x86_64",
        target_env = "msvc"
    )) {
        Ok("x86_64-pc-windows-msvc")
    } else {
        Err(Error::InvalidInput(
            "self-update is unavailable for this platform; install a supported release manually"
                .into(),
        ))
    }
}

fn names() -> [&'static str; 2] {
    if cfg!(windows) {
        ["solascope.exe", "sas.exe"]
    } else {
        ["solascope", "sas"]
    }
}

async fn perform(
    source: &download::Source,
    current: &Version,
    target: &str,
    options: &Options,
    executable: Option<&Path>,
) -> Result<Report> {
    let release = if let Some(archive) = &options.archive {
        let name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| failure("invalid local archive filename"))?;
        let suffix = if target.contains("windows") {
            ".zip"
        } else {
            ".tar.gz"
        };
        let version = name
            .strip_prefix("solascope-")
            .and_then(|name| name.strip_suffix(&format!("-{target}{suffix}")))
            .ok_or_else(|| {
                failure("local archive must be named solascope-VERSION-TARGET.tar.gz (or .zip)")
            })?;
        let version = Version::parse(version)
            .map_err(|_| failure("invalid version in local archive filename"))?;
        if !version.pre.is_empty() {
            return Err(failure("update accepts stable release archives only"));
        }
        download::Release::local(archive, version, target)?
    } else {
        source.latest(target).await?
    };
    // Build metadata does not change SemVer precedence. Never downgrade a newer build.
    let available = release.version.cmp_precedence(current).is_gt();
    let mut report = Report {
        status: if available {
            "update_available"
        } else {
            "up_to_date"
        },
        current_version: current.to_string(),
        latest_version: release.version.to_string(),
        target: target.into(),
        release_url: format!(
            "https://github.com/wchiway/SolaScope/releases/tag/{}",
            release.tag
        ),
        directory: None,
        warnings: Vec::new(),
    };
    if options.check
        || (!available && !(options.force && release.version.cmp_precedence(current).is_eq()))
    {
        return Ok(report);
    }
    let executable = match executable {
        Some(path) => path.to_owned(),
        None => std::env::current_exe()?,
    };
    let directory = installation_directory(&executable)?;
    let _lock = UpdateLock::acquire(&directory)?;
    validate_binaries(&directory)?;
    let before = binary_fingerprints(&directory)?;
    let stage_parent = staging_parent(&directory);
    let stage = tempfile::Builder::new().prefix(".solascope-update-").tempdir_in(stage_parent)
        .map_err(|error| failure(format!("cannot stage update in {}: {error}; choose a writable installation, no sudo is invoked", directory.display())))?;
    if let (Some(archive), Some(checksum)) = (&options.archive, &options.checksum) {
        download::stage_archive(archive, checksum, &release.version, target, stage.path())?;
    } else {
        source.stage(&release, stage.path()).await?;
    }
    for name in names() {
        make_executable(&stage.path().join(name))?;
        probe(&stage.path().join(name), &release.version).await?;
    }
    // Also detect writers that do not participate in the installation lock.
    validate_binaries(&directory)?;
    if before != binary_fingerprints(&directory)? {
        return Err(failure(
            "installed binaries changed while staging; retry the update",
        ));
    }
    if let Err(error) = publish(&directory, stage.path(), |from, to| fs::rename(from, to)) {
        let recovery = stage.keep();
        return Err(failure(format!(
            "{error}; update files/backups retained at {}",
            recovery.display()
        )));
    }
    report.status = "updated";
    report.directory = Some(directory);
    close_stage(stage, &mut report.warnings);
    Ok(report)
}

fn reject_links(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(failure(format!(
                "self-update refuses symlink/reparse paths: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

fn installation_directory(executable: &Path) -> Result<PathBuf> {
    if !executable.is_absolute()
        || !names()
            .iter()
            .any(|name| executable.file_name() == Some(std::ffi::OsStr::new(name)))
    {
        return Err(failure(
            "self-update requires an installed solascope or sas executable",
        ));
    }
    reject_links(executable)?;
    let directory = executable
        .parent()
        .ok_or_else(|| failure("executable has no parent directory"))?;
    if directory
        .ancestors()
        .any(|path| path.file_name().is_some_and(|name| name == "target"))
    {
        return Err(failure(
            "self-update refuses Cargo target/build directories; install both binaries first",
        ));
    }
    validate_binaries(directory)?;
    Ok(directory.to_path_buf())
}

fn validate_binaries(directory: &Path) -> Result<()> {
    reject_links(directory)?;
    for name in names() {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            failure(format!(
                "both CLI entry points must be installed together; {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(failure(format!(
                "refusing non-regular installation binary {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn binary_fingerprints(directory: &Path) -> Result<Vec<[u8; 32]>> {
    use std::io::Read;
    names()
        .iter()
        .map(|name| {
            let mut file = File::open(directory.join(name))?;
            let mut digest = Sha256::new();
            let mut bytes = [0; 64 * 1024];
            loop {
                let size = file.read(&mut bytes)?;
                if size == 0 {
                    break;
                }
                digest.update(&bytes[..size]);
            }
            Ok(digest.finalize().into())
        })
        .collect()
}

struct UpdateLock {
    _file: File,
    // Unix install.sh uses this same directory lock, Windows uses the hashed file.
    directory_lock: Option<PathBuf>,
}
impl UpdateLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let (lock_path, directory_lock) = lock_paths(directory);
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse(&metadata) {
                return Err(failure("update lock must be a regular file"));
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0); // Match install.ps1's FileShare.None, including cross-tool locking.
        }
        let file = options.open(&lock_path)?;
        file.try_lock_exclusive()
            .map_err(|_| failure("another installer/update is active; retry after it finishes"))?;
        if let Some(path) = &directory_lock {
            fs::create_dir(path).map_err(|error| failure(format!("installation lock {} is active or inaccessible: {error}; inspect stale locks before removing them", path.display())))?;
        }
        Ok(Self {
            _file: file,
            directory_lock,
        })
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        if let Some(path) = &self.directory_lock {
            let _ = fs::remove_dir(path);
        }
    }
}

fn staging_parent(directory: &Path) -> &Path {
    #[cfg(windows)]
    {
        // Keep locked old executables outside installer-owned trees so reinstall
        // does not reject update backups as unrelated installation contents.
        if directory.file_name().is_some_and(|name| name == "bin") {
            if let Some(root) = directory
                .parent()
                .filter(|root| root.join(".solascope-install.json").is_file())
            {
                return root.parent().unwrap_or(root);
            }
        }
    }
    directory
}

fn lock_paths(directory: &Path) -> (PathBuf, Option<PathBuf>) {
    #[cfg(windows)]
    {
        // install.ps1 locks the installation root (one level above bin).
        let root = if directory.file_name().is_some_and(|name| name == "bin")
            && directory
                .parent()
                .is_some_and(|root| root.join(".solascope-install.json").is_file())
        {
            directory.parent().unwrap()
        } else {
            directory
        };
        // current_exe uses extended paths; the PowerShell installer hashes DOS paths.
        let text = root
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_uppercase();
        let hash = hex::encode_upper(Sha256::digest(text.as_bytes()));
        (
            root.parent()
                .unwrap_or(root)
                .join(format!(".solascope-lock-{hash}")),
            None,
        )
    }
    #[cfg(not(windows))]
    {
        (
            directory.join(".solascope-update.lock"),
            Some(directory.join(".solascope-install.lock")),
        )
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<()> {
    Ok(())
}

async fn probe(path: &Path, expected: &Version) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut process = tokio::process::Command::new(path);
    process
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .map_err(|error| failure(format!("cannot execute staged {}: {error}", path.display())))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("missing version probe stdout"))?;
    let probe = async {
        let mut bytes = Vec::new();
        stdout.take(1025).read_to_end(&mut bytes).await?;
        if bytes.len() > 1024 {
            return Err(io::Error::other("version output exceeds limit"));
        }
        let status = child.wait().await?;
        Ok::<_, io::Error>((status, bytes))
    };
    let (status, bytes) = match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(result)) => result,
        error => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(failure(format!(
                "staged binary --version failed or timed out; old installation preserved ({})",
                if error.is_err() {
                    "timeout"
                } else {
                    "invalid output"
                }
            )));
        }
    };
    let expected_line = format!("solascope {expected}");
    if !status.success()
        || (bytes != format!("{expected_line}\n").as_bytes()
            && bytes != format!("{expected_line}\r\n").as_bytes())
    {
        return Err(failure(format!(
            "staged {} did not report {expected_line}; old installation preserved",
            path.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    Ok(())
}

// Move both names with same-filesystem backups. The pair is sequential, not atomic.
// On Windows the active old executable may remain mapped until this process exits.
fn publish(
    directory: &Path,
    stage: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let mut moved_old = Vec::new();
    let mut moved_new = Vec::new();
    let result = (|| -> io::Result<()> {
        for name in names() {
            rename(&directory.join(name), &stage.join(format!("old-{name}")))?;
            moved_old.push(name);
            rename(&stage.join(name), &directory.join(name))?;
            moved_new.push(name);
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for name in moved_old.iter().rev() {
            if moved_new.contains(name) {
                if let Err(error) = rename(&directory.join(name), &stage.join(name)) {
                    rollback_errors.push(format!("remove new {name}: {error}"));
                    continue;
                }
            }
            if let Err(error) = rename(&stage.join(format!("old-{name}")), &directory.join(name)) {
                rollback_errors.push(format!("restore {name}: {error}"));
            }
        }
        return Err(failure(format!("cannot replace both CLI binaries: {error}. {} Close other MCP/watch processes and retry", if rollback_errors.is_empty() { "Previous binaries restored.".into() } else { format!("Rollback incomplete: {}.", rollback_errors.join("; ")) })));
    }
    Ok(())
}

fn close_stage(stage: TempDir, warnings: &mut Vec<String>) {
    let path = stage.path().to_owned();
    if let Err(error) = stage.close() {
        warnings.push(format!("update succeeded; old running binaries/backups could not be removed at {}: {error}. After exiting this process and closing old MCP/watch sessions, remove that directory", path.display()));
    }
}

#[cfg(test)]
mod check_tests;
#[cfg(test)]
mod tests;
