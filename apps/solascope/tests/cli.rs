use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

const SOLASCOPE: &str = env!("CARGO_BIN_EXE_solascope");
const SAS: &str = env!("CARGO_BIN_EXE_sas");

fn command(binary: &str, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("user-config"))
        .env("HOME", root.join("user-home"))
        .env("USERPROFILE", root.join("user-home"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SOLASCOPE_") {
            command.env_remove(key);
        }
    }
    // Do not inherit an unrelated project marker from a shared temporary parent.
    command.env("SOLASCOPE__PROJECT__ROOT", root);
    command
}

fn json_ok(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON result")
}

fn fixture() -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("fixture.rs"),
        "pub fn sparkling_fixture_symbol() -> usize {\n    42\n}\n",
    )
    .unwrap();
    directory
}

#[test]
fn both_names_have_identical_help_and_version_without_config_or_database() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["index", "--help"],
        vec!["search", "--help"],
        vec!["context", "--help"],
        vec!["mcp", "--help"],
    ] {
        let long = command(SOLASCOPE, root.path())
            .args(&args)
            .output()
            .unwrap();
        let short = command(SAS, root.path()).args(&args).output().unwrap();
        assert!(long.status.success());
        assert!(short.status.success());
        assert_eq!(long.stdout, short.stdout);
        assert!(long.stderr.is_empty());
        assert!(short.stderr.is_empty());
        assert!(String::from_utf8_lossy(&long.stdout).contains("solascope"));
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn init_json_reports_path_and_does_not_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let value = json_ok(
        command(SAS, root.path())
            .args(["init", "--json"])
            .output()
            .unwrap(),
    );
    let config = value["config_path"].as_str().unwrap();
    assert_eq!(
        Path::new(config),
        root.path()
            .join("user-home/.solascope/.solascope.toml")
            .canonicalize()
            .unwrap()
    );
    assert!(!root.path().join(".solascope.toml").exists());
    let original = std::fs::read(config).unwrap();
    assert!(!std::str::from_utf8(&original)
        .unwrap()
        .lines()
        .any(|line| line.starts_with("root = ") || line.starts_with("index_dir = ")));
    let second = command(SOLASCOPE, root.path())
        .args(["init", "--json"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(second.stdout.is_empty());
    assert_eq!(std::fs::read(config).unwrap(), original);
}

#[test]
fn user_initialization_is_shared_but_index_roots_remain_project_specific() {
    let home = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for (root, name) in [
        (first.path(), "first_project"),
        (second.path(), "second_project"),
    ] {
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("lib.rs"), format!("fn {name}() {{}}\n")).unwrap();
    }
    let initialized = json_ok(
        command(SAS, first.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .args(["init", "--json"])
            .output()
            .unwrap(),
    );
    let expected = home
        .path()
        .join(".solascope/.solascope.toml")
        .canonicalize()
        .unwrap();
    assert_eq!(
        Path::new(initialized["config_path"].as_str().unwrap()),
        expected
    );
    for (root, name) in [
        (first.path(), "first_project"),
        (second.path(), "second_project"),
    ] {
        let indexed = json_ok(
            command(SOLASCOPE, root)
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["index", "--no-watch", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(indexed["files_total"], 1);
        let found = json_ok(
            command(SAS, root)
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["symbol", name, "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(found["results"][0]["symbol"], name);
        assert!(!root.join(".solascope.toml").exists());
    }
}

#[test]
fn lexical_workflow_runs_without_models_and_aliases_return_identical_data() {
    let root = fixture();
    let report = json_ok(
        command(SOLASCOPE, root.path())
            .args(["index", "--json"])
            .output()
            .unwrap(),
    );
    assert!(report["chunks"].as_u64().unwrap() > 0);
    let args = [
        "search",
        "sparkling_fixture_symbol",
        "--mode",
        "lexical",
        "--no-rerank",
        "--json",
    ];
    let mut long = json_ok(command(SOLASCOPE, root.path()).args(args).output().unwrap());
    let mut short = json_ok(command(SAS, root.path()).args(args).output().unwrap());
    // Everything but the per-run timings must be identical.
    for response in [&mut long, &mut short] {
        let timings = response.as_object_mut().unwrap().remove("timings").unwrap();
        assert!(timings["total_ms"].as_f64().unwrap() >= timings["retrieval_ms"].as_f64().unwrap());
    }
    assert_eq!(long, short);
    assert!(!long["results"].as_array().unwrap().is_empty());
    assert_eq!(long["results"][0]["relative_path"], "fixture.rs");
    json_ok(
        command(SAS, root.path())
            .args(["status", "--json"])
            .output()
            .unwrap(),
    );
    let symbols = json_ok(
        command(SAS, root.path())
            .args([
                "symbol",
                "sparkling_fixture_symbol",
                "--kind",
                "function",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert!(!symbols["results"].as_array().unwrap().is_empty());
    let center = json_ok(
        command(SAS, root.path())
            .args([
                "context",
                "fixture.rs:2",
                "--before",
                "1",
                "--after",
                "1",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let range = json_ok(
        command(SOLASCOPE, root.path())
            .args([
                "context",
                "fixture.rs",
                "--start-line",
                "1",
                "--end-line",
                "3",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(center, range);
    assert_eq!(center["lines"].as_array().unwrap().len(), 3);
    let human = command(SAS, root.path())
        .args(["context", "fixture.rs:2"])
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("fixture.rs:2:"));
    json_ok(
        command(SAS, root.path())
            .args(["doctor", "--json"])
            .output()
            .unwrap(),
    );
    json_ok(
        command(SAS, root.path())
            .args(["doctor", "--prune-cache", "--json"])
            .output()
            .unwrap(),
    );
}

#[test]
fn positional_index_path_takes_precedence_over_root() {
    let ignored = tempfile::tempdir().unwrap();
    let root = fixture();
    json_ok(
        command(SAS, ignored.path())
            .args([
                "--root",
                ignored.path().to_str().unwrap(),
                "index",
                root.path().to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let result = json_ok(
        command(SAS, ignored.path())
            .args([
                "--root",
                root.path().to_str().unwrap(),
                "search",
                "sparkling_fixture_symbol",
                "--mode",
                "lexical",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert!(!result["results"].as_array().unwrap().is_empty());
}

#[test]
fn default_index_recursively_scans_cwd_instead_of_toml_root() {
    for binary in [SOLASCOPE, SAS] {
        let root = fixture();
        std::fs::create_dir(root.path().join("configured")).unwrap();
        std::fs::create_dir_all(root.path().join("nested/level2")).unwrap();
        std::fs::write(
            root.path().join(".solascope.toml"),
            "[project]\nroot='configured'\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("configured/configured.rs"),
            "pub fn configured_fixture_symbol() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("nested/level2/deep.rs"),
            "pub fn recursive_nested_symbol() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("nested/notes.txt"),
            "recursive plain text fixture\n",
        )
        .unwrap();
        let report = json_ok(
            command(binary, root.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["index", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(report["files_total"], 4);
        let symbols = json_ok(
            command(binary, root.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["symbol", "recursive_nested_symbol", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            symbols["results"][0]["relative_path"],
            "nested/level2/deep.rs"
        );
    }
}

#[test]
fn default_index_keeps_cwd_while_loading_parent_configuration() {
    for binary in [SOLASCOPE, SAS] {
        let root = fixture();
        let cwd = root.path().join("src/deep");
        std::fs::create_dir_all(cwd.join("nested/level2")).unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(
            root.path().join(".solascope.toml"),
            "[project]\nroot='.'\n[search]\nlimit=1\n",
        )
        .unwrap();
        for path in ["local.rs", "nested/level2/deep.rs"] {
            std::fs::write(cwd.join(path), "pub fn inherited_config_symbol() {}\n").unwrap();
        }
        let report = json_ok(
            command(binary, &cwd)
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["index", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(report["files_total"], 2);
        assert!(cwd.join(".solascope").is_dir());
        assert!(!root.path().join(".solascope").exists());
        let symbols = json_ok(
            command(binary, &cwd)
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["symbol", "inherited_config_symbol", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(symbols["results"].as_array().unwrap().len(), 1);
        let status = json_ok(
            command(binary, &cwd)
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["status", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            status["root"],
            cwd.canonicalize().unwrap().to_str().unwrap()
        );
    }
}

#[test]
fn default_index_uses_external_configuration_without_changing_cwd() {
    for binary in [SOLASCOPE, SAS] {
        let root = fixture();
        let settings = tempfile::tempdir().unwrap();
        let config = settings.path().join("custom.toml");
        std::fs::write(
            &config,
            "[project]\nroot='missing-source-directory'\n[search]\nlimit=1\n",
        )
        .unwrap();
        let report = json_ok(
            command(binary, root.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args(["index", "--config", config.to_str().unwrap(), "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(report["files_total"], 1);
        assert!(root.path().join(".solascope").is_dir());
        assert!(!settings.path().join(".solascope").exists());
        let symbols = json_ok(
            command(binary, root.path())
                .env_remove("SOLASCOPE__PROJECT__ROOT")
                .args([
                    "symbol",
                    "sparkling_fixture_symbol",
                    "--config",
                    config.to_str().unwrap(),
                    "--json",
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(symbols["results"][0]["relative_path"], "fixture.rs");
    }
}

#[test]
fn invalid_arguments_are_stderr_only_and_use_core_input_exit_code() {
    let root = fixture();
    let excessive = "x".repeat(4097);
    for args in [
        vec!["search", "hello", "--limit", "0"],
        vec!["search", "hello", "--limit", "101"],
        vec!["search", "hello", "--mode", "unknown"],
        vec!["search", "   "],
        vec!["search", excessive.as_str()],
        vec!["context", "fixture.rs:0"],
        vec!["context", "fixture.rs", "--start", "1"],
        vec!["context", "fixture.rs", "--start", "1", "--end", "201"],
        vec!["context", "fixture.rs:1", "--line", "2"],
        vec![
            "context",
            "fixture.rs:100",
            "--before",
            "100",
            "--after",
            "100",
        ],
    ] {
        let output = command(SAS, root.path())
            .args(&args)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(solascope_core::Error::InvalidInput(String::new()).exit_code()),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn missing_index_uses_typed_exit_code_and_json_error_goes_to_stderr() {
    let root = fixture();
    let output = command(SAS, root.path())
        .args(["search", "fixture", "--mode", "lexical", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(solascope_core::Error::IndexMissing.exit_code())
    );
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("index"));
}
