//! Run self-update from copies of the real binaries, never Cargo's build outputs.
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Cursor, Write},
    path::Path,
    process::Command,
};

const SOLASCOPE: &str = env!("CARGO_BIN_EXE_solascope");
const SAS: &str = env!("CARGO_BIN_EXE_sas");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PUBLIC: &[&str] = &[
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
];

fn target() -> &'static str {
    if cfg!(windows) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            concat!("a", "arch64-apple-darwin")
        } else {
            "x86_64-apple-darwin"
        }
    } else {
        "x86_64-unknown-linux-gnu"
    }
}
fn names() -> [&'static str; 2] {
    if cfg!(windows) {
        ["solascope.exe", "sas.exe"]
    } else {
        ["solascope", "sas"]
    }
}
fn fixture_archive(directory: &Path) -> std::path::PathBuf {
    let mut entries: Vec<(String, Vec<u8>)> = PUBLIC
        .iter()
        .map(|name| ((*name).into(), b"public fixture".to_vec()))
        .collect();
    let installers: &[&str] = if cfg!(windows) {
        &["install.ps1", "install.cmd"]
    } else {
        &["install.sh"]
    };
    entries.extend(
        installers
            .iter()
            .map(|name| ((*name).into(), b"installer is never executed".to_vec())),
    );
    // Cargo debug binaries include large DWARF sections and are not release
    // payloads. A small native Rust fixture exercises real executable probes;
    // packaging CI separately updates from the full native release archive.
    let source = directory.join("version_fixture.rs");
    let executable = directory.join(if cfg!(windows) {
        "version_fixture.exe"
    } else {
        "version_fixture"
    });
    fs::write(
        &source,
        format!("fn main() {{ println!(\"solascope {VERSION}\"); }}"),
    )
    .unwrap();
    let status = Command::new("rustc")
        .arg("--crate-name")
        .arg("version_fixture")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(status.success(), "could not compile native version fixture");
    let bytes = fs::read(executable).unwrap();
    for name in names() {
        entries.push((name.into(), bytes.clone()));
    }
    let archive = directory.join(format!(
        "solascope-{VERSION}-{}{}",
        target(),
        if cfg!(windows) { ".zip" } else { ".tar.gz" }
    ));
    if cfg!(windows) {
        let mut output = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        for (name, data) in entries {
            output
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated)
                        .unix_permissions(0o755),
                )
                .unwrap();
            output.write_all(&data).unwrap();
        }
        output.finish().unwrap();
    } else {
        let gzip = flate2::write::GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut output = tar::Builder::new(gzip);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            output
                .append_data(&mut header, name, Cursor::new(data))
                .unwrap();
        }
        output.into_inner().unwrap().finish().unwrap();
    }
    let digest = hex::encode(Sha256::digest(fs::read(&archive).unwrap()));
    fs::write(
        directory.join("SHA256SUMS"),
        format!(
            "{digest}  {}\n",
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();
    archive
}
fn invoke(binary: &Path, root: &Path, archive: &Path) -> std::process::Output {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .args(["--json", "--config"])
        .arg(root.join("broken.toml"))
        .args(["update", "--force", "--archive"])
        .arg(archive)
        .arg("--checksum")
        .arg(root.join("SHA256SUMS"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SOLASCOPE_") {
            command.env_remove(key);
        }
    }
    command.output().unwrap()
}

#[test]
fn both_real_entrypoints_self_replace_offline_without_loading_broken_config() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let install = root.path().join("installed bin with spaces");
    fs::create_dir(&install).unwrap();
    for (name, source) in names().iter().zip([SOLASCOPE, SAS]) {
        fs::copy(source, install.join(name)).unwrap();
    }
    fs::write(root.path().join("broken.toml"), "invalid = [").unwrap();
    fs::write(install.join("unrelated"), "keep").unwrap();
    let archive = fixture_archive(root.path());
    for name in names() {
        // Restore originals so each alias actually runs its updater; the tiny
        // replacement fixture intentionally implements only --version.
        for (binary, source) in names().iter().zip([SOLASCOPE, SAS]) {
            fs::copy(source, install.join(binary)).unwrap();
        }
        let output = invoke(&install.join(name), root.path(), &archive);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "updated");
        for binary in names() {
            let output = Command::new(install.join(binary))
                .arg("--version")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                format!("solascope {VERSION}")
            );
        }
    }
    assert_eq!(
        fs::read_to_string(install.join("unrelated")).unwrap(),
        "keep"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("broken.toml")).unwrap(),
        "invalid = ["
    );
    assert!(!root.path().join(".solascope").exists());
}

#[test]
fn invalid_checksum_does_not_change_installed_entrypoints() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let install = root.path().join("bin");
    fs::create_dir(&install).unwrap();
    for (name, source) in names().iter().zip([SOLASCOPE, SAS]) {
        fs::copy(source, install.join(name)).unwrap();
    }
    let archive = fixture_archive(root.path());
    fs::write(
        root.path().join("SHA256SUMS"),
        format!(
            "{}  {}\n",
            "0".repeat(64),
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();
    let before = fs::read(install.join(names()[0])).unwrap();
    let output = invoke(&install.join(names()[0]), root.path(), &archive);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(install.join(names()[0])).unwrap(), before);
}
