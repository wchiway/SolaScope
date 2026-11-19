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
        .env("XDG_CONFIG_HOME", root.join("user-config"));
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
    let original = std::fs::read(config).unwrap();
    let second = command(SOLASCOPE, root.path())
        .args(["init", "--json"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(second.stdout.is_empty());
    assert_eq!(std::fs::read(config).unwrap(), original);
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
