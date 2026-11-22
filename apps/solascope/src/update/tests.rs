use super::*;

fn installation() -> TempDir {
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    for name in names() {
        fs::write(directory.path().join(name), format!("old {name}")).unwrap();
    }
    directory
}

#[test]
fn publication_replaces_both_names_and_keeps_backups() {
    let directory = installation();
    let stage = tempfile::tempdir_in(directory.path()).unwrap();
    for name in names() {
        fs::write(stage.path().join(name), format!("new {name}")).unwrap();
    }
    publish(directory.path(), stage.path(), |from, to| {
        fs::rename(from, to)
    })
    .unwrap();
    for name in names() {
        assert_eq!(
            fs::read_to_string(directory.path().join(name)).unwrap(),
            format!("new {name}")
        );
        assert_eq!(
            fs::read_to_string(stage.path().join(format!("old-{name}"))).unwrap(),
            format!("old {name}")
        );
    }
}

#[test]
fn second_binary_failure_rolls_back_both_originals() {
    let directory = installation();
    let stage = tempfile::tempdir_in(directory.path()).unwrap();
    for name in names() {
        fs::write(stage.path().join(name), "new").unwrap();
    }
    let error = publish(directory.path(), stage.path(), |from, to| {
        if from == stage.path().join(names()[1]) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "locked fixture",
            ));
        }
        fs::rename(from, to)
    })
    .unwrap_err();
    assert!(error.to_string().contains("Previous binaries restored"));
    for name in names() {
        assert_eq!(
            fs::read_to_string(directory.path().join(name)).unwrap(),
            format!("old {name}")
        );
    }
}

#[test]
fn rollback_failure_reports_recovery_without_deleting_backup() {
    let directory = installation();
    let stage = tempfile::tempdir_in(directory.path()).unwrap();
    for name in names() {
        fs::write(stage.path().join(name), "new").unwrap();
    }
    let error = publish(directory.path(), stage.path(), |from, to| {
        if from == stage.path().join(names()[1])
            || from == stage.path().join(format!("old-{}", names()[1]))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "locked fixture",
            ));
        }
        fs::rename(from, to)
    })
    .unwrap_err();
    assert!(error.to_string().contains("Rollback incomplete"));
    assert_eq!(
        fs::read_to_string(stage.path().join(format!("old-{}", names()[1]))).unwrap(),
        format!("old {}", names()[1])
    );
}

#[test]
fn installation_requires_two_regular_names_and_rejects_build_outputs() {
    let directory = installation();
    assert!(installation_directory(&directory.path().join(names()[0])).is_ok());
    fs::remove_file(directory.path().join(names()[1])).unwrap();
    assert!(installation_directory(&directory.path().join(names()[0])).is_err());
    let built = directory.path().join("target/release");
    fs::create_dir_all(&built).unwrap();
    for name in names() {
        fs::write(built.join(name), "fixture").unwrap();
    }
    assert!(installation_directory(&built.join(names()[0]))
        .unwrap_err()
        .to_string()
        .contains("Cargo target"));
}

#[cfg(unix)]
#[test]
fn installation_refuses_binary_symlinks_and_shared_installer_lock() {
    use std::os::unix::fs::symlink;
    let directory = installation();
    let linked = directory.path().join(names()[1]);
    fs::remove_file(&linked).unwrap();
    symlink(directory.path().join(names()[0]), &linked).unwrap();
    assert!(validate_binaries(directory.path()).is_err());
    fs::create_dir(directory.path().join(".solascope-install.lock")).unwrap();
    assert!(UpdateLock::acquire(directory.path()).is_err());
}

#[test]
fn exclusive_lock_serializes_updaters() {
    let directory = installation();
    let lock = UpdateLock::acquire(directory.path()).unwrap();
    assert!(UpdateLock::acquire(directory.path()).is_err());
    drop(lock);
    assert!(UpdateLock::acquire(directory.path()).is_ok());
}

#[cfg(windows)]
#[test]
fn installer_owned_backups_remain_outside_owned_tree_and_lock_matches_powershell() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owned = root.path().join("SolaScope");
    let bin = owned.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(owned.join(".solascope-install.json"), "{}").unwrap();
    assert_eq!(staging_parent(&bin), root.path());
    let text = owned
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_uppercase();
    let expected = root.path().join(format!(
        ".solascope-lock-{}",
        hex::encode_upper(Sha256::digest(text.as_bytes()))
    ));
    assert_eq!(lock_paths(&bin), (expected, None));
}

#[cfg(unix)]
#[tokio::test]
async fn probes_reject_wrong_versions_and_excess_output() {
    let directory = installation();
    let binary = directory.path().join("probe");
    let expected = Version::parse("1.2.3").unwrap();
    for (body, valid) in [
        ("printf 'solascope 1.2.3\\n'", true),
        ("printf 'solascope 9.9.9\\n'", false),
        ("printf 'solascope 1.2.3\\nextra\\n'", false),
        ("while :; do printf 'excess output'; done", false),
    ] {
        fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        make_executable(&binary).unwrap();
        assert_eq!(probe(&binary, &expected).await.is_ok(), valid);
    }
}
