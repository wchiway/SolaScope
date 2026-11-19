//! The `solascope-bench` binary end to end: arguments, the split between
//! stdout and stderr, and exit codes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const BENCH: &str = env!("CARGO_BIN_EXE_solascope-bench");

fn fixture_queries() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/multilang/queries.json")
}

/// A copy of the fixture sources (without the annotations) plus a scratch
/// directory that stands in for the user's configuration home.
struct Sandbox {
    project: tempfile::TempDir,
    scratch: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let project = tempfile::tempdir().unwrap();
        let fixture = fixture_queries().parent().unwrap().to_path_buf();
        for entry in std::fs::read_dir(fixture).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() != "queries.json" {
                std::fs::copy(entry.path(), project.path().join(entry.file_name())).unwrap();
            }
        }
        Self {
            project,
            scratch: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> &str {
        self.project.path().to_str().unwrap()
    }

    /// The binary loads configuration the way the CLI does, so the user's own
    /// configuration file and `SOLASCOPE__*` variables are kept out.
    fn bench(&self) -> Command {
        let mut command = Command::new(BENCH);
        command
            .current_dir(self.scratch.path())
            .env("XDG_CONFIG_HOME", self.scratch.path().join("config"))
            .env("HOME", self.scratch.path().join("home"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("SOLASCOPE_") {
                command.env_remove(key);
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.bench().args(args).output().unwrap()
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.project.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}\nstderr: {}",
        output.status.code(),
        text(&output.stderr)
    );
}

/// stdout must hold exactly one JSON document on one line.
fn single_json(output: &Output) -> Value {
    succeeded(output);
    let stdout = text(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    serde_json::from_str(&stdout).expect("stdout must be one JSON document")
}

#[test]
fn help_describes_both_subcommands_and_their_flags() {
    let sandbox = Sandbox::new();
    let top = sandbox.run(&["--help"]);
    succeeded(&top);
    assert!(top.stderr.is_empty());
    let top = text(&top.stdout);
    assert!(top.contains("quality") && top.contains("perf"), "{top}");

    let version = sandbox.run(&["--version"]);
    succeeded(&version);
    assert_eq!(
        text(&version.stdout).trim(),
        format!("solascope-bench {}", env!("CARGO_PKG_VERSION"))
    );

    let quality = sandbox.run(&["quality", "--help"]);
    succeeded(&quality);
    let quality = text(&quality.stdout);
    for flag in [
        "--root <DIR>",
        "--queries <FILE>",
        "--config <TOML>",
        "--limit <LIMIT>",
        "--no-index",
        "--json",
        "[default: 10]",
    ] {
        assert!(quality.contains(flag), "missing {flag} in:\n{quality}");
    }

    let perf = sandbox.run(&["perf", "--help"]);
    succeeded(&perf);
    let perf = text(&perf.stdout);
    for flag in [
        "--root <DIR>",
        "--queries <FILE>",
        "--config <TOML>",
        "--runs <RUNS>",
        "--keep-index",
        "--json",
        "[default: 30]",
    ] {
        assert!(perf.contains(flag), "missing {flag} in:\n{perf}");
    }
    // Asking for help does not touch the project.
    assert!(!sandbox.entries().contains(&".solascope".to_owned()));
}

#[test]
fn quality_prints_tables_by_default_and_one_json_document_on_request() {
    let sandbox = Sandbox::new();
    let queries = fixture_queries();
    let base = [
        "quality",
        "--root",
        sandbox.root(),
        "--queries",
        queries.to_str().unwrap(),
    ];

    let tables = sandbox.run(&base);
    succeeded(&tables);
    let stdout = text(&tables.stdout);
    let stderr = text(&tables.stderr);
    for expected in [
        "# SolaScope retrieval quality",
        "## Metrics",
        "| lexical | all        |      18 | 1.000 |    1.000 |  1.000 |",
        "## Skipped variants",
        "## Misses (expected location not ranked first)",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    // Progress and skip notices stay off stdout.
    assert!(!stdout.contains("quality: "), "{stdout}");
    for expected in [
        "quality: updating the index of ",
        "quality: lexical: running 18 queries",
        "quality: skipping semantic: ",
        "quality: skipping hybrid+rerank: ",
    ] {
        assert!(
            stderr.contains(expected),
            "missing {expected:?} in:\n{stderr}"
        );
    }

    let mut with_json = base.to_vec();
    with_json.push("--json");
    let json = sandbox.run(&with_json);
    let document = single_json(&json);
    assert_eq!(document["benchmark"], "quality");
    assert_eq!(document["query_count"], 18);
    assert_eq!(document["variants"][0]["variant"], "lexical");
    assert_eq!(document["variants"][0]["overall"]["top1"], 1.0);
    assert_eq!(document["variants"][0]["overall"]["recall_at_5"], 1.0);
    assert_eq!(document["variants"][0]["overall"]["mrr_at_10"], 1.0);
    assert_eq!(document["skipped_variants"].as_array().unwrap().len(), 2);
    // The second run found the index of the first one up to date.
    assert_eq!(document["index"]["indexed"], true);
    assert_eq!(document["index"]["report"]["files_updated"], 0);
    assert!(text(&json.stderr).contains("quality: skipping semantic: "));

    with_json.extend(["--no-index", "--limit", "5"]);
    let reused = single_json(&sandbox.run(&with_json));
    assert_eq!(reused["index"]["indexed"], false);
    assert_eq!(reused["limit"], 5);
    assert_eq!(reused["variants"][0]["overall"]["recall_at_5"], 1.0);
    assert!(reused["notices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|notice| notice.as_str().unwrap().contains("limit 5 is below 10")));
}

#[test]
fn perf_measures_latency_only_with_queries_and_never_touches_the_project() {
    let sandbox = Sandbox::new();
    let queries = fixture_queries();
    let before = sandbox.entries();

    let json = sandbox.run(&[
        "perf",
        "--root",
        sandbox.root(),
        "--queries",
        queries.to_str().unwrap(),
        "--runs",
        "2",
        "--json",
    ]);
    let document = single_json(&json);
    assert_eq!(document["benchmark"], "perf");
    assert_eq!(document["index_phases"].as_array().unwrap().len(), 3);
    assert_eq!(document["index_phases"][0]["report"]["files_total"], 5);
    assert_eq!(document["latency"]["runs"], 2);
    assert_eq!(document["latency"]["query_count"], 18);
    assert!(document["latency"]["scope"]
        .as_str()
        .unwrap()
        .contains("excludes process start-up"));
    assert_eq!(document["index_dir_kept"], false);
    assert!(!Path::new(document["index_dir"].as_str().unwrap()).exists());
    assert!(text(&json.stderr).contains("perf: temporary index directory "));
    assert_eq!(
        sandbox.entries(),
        before,
        "perf changed the project directory"
    );

    let tables = sandbox.run(&["perf", "--root", sandbox.root(), "--keep-index"]);
    succeeded(&tables);
    let stdout = text(&tables.stdout);
    let stderr = text(&tables.stderr);
    for expected in [
        "# SolaScope performance",
        "## Indexing",
        "| first index |",
        "## Peak resident memory",
        "## Search latency\n\nskipped: no query file was given (--queries)",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(!stdout.contains("perf: "), "{stdout}");
    assert!(
        stderr.contains("perf: skipping search latency: no query file was given (--queries)"),
        "{stderr}"
    );
    assert_eq!(
        sandbox.entries(),
        before,
        "perf changed the project directory"
    );
    // --keep-index leaves the temporary index where the report says it is.
    let kept = stdout
        .lines()
        .find_map(|line| line.strip_prefix("index dir"))
        .and_then(|rest| rest.trim().strip_suffix(" (temporary; kept)"))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("no kept index directory in:\n{stdout}"));
    assert!(kept.join("index.sqlite3").is_file(), "{}", kept.display());
    std::fs::remove_dir_all(&kept).unwrap();
}

#[test]
fn perf_accepts_plain_string_arrays_and_deduplicates_them() {
    let sandbox = Sandbox::new();
    let queries = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        queries.path(),
        r#"["readSession", "readSession", "INSUFFICIENT_STOCK"]"#,
    )
    .unwrap();
    let before = sandbox.entries();
    let output = sandbox.run(&[
        "perf",
        "--root",
        sandbox.root(),
        "--queries",
        queries.path().to_str().unwrap(),
        "--runs",
        "1",
        "--json",
    ]);
    let document = single_json(&output);
    assert_eq!(document["latency"]["query_count"], 2);
    assert_eq!(
        document["latency"]["variants"][0]["queries"][0]["query"],
        "readSession"
    );
    assert_eq!(
        document["latency"]["variants"][0]["queries"][1]["query"],
        "INSUFFICIENT_STOCK"
    );
    assert_eq!(sandbox.entries(), before);
}

#[test]
fn failures_use_the_core_exit_codes_and_leave_stdout_empty() {
    let sandbox = Sandbox::new();
    let queries = fixture_queries();
    let queries = queries.to_str().unwrap();
    let root = sandbox.root();
    let missing_root = sandbox.scratch.path().join("no-such-project");
    let missing_root = missing_root.to_str().unwrap();
    let malformed = sandbox.scratch.path().join("malformed.json");
    std::fs::write(
        &malformed,
        r#"[{"query": "q", "path": "p", "symbol": "s"}]"#,
    )
    .unwrap();
    let malformed = malformed.to_str().unwrap();
    let empty = sandbox.scratch.path().join("empty.json");
    std::fs::write(&empty, "[]").unwrap();
    let empty = empty.to_str().unwrap();

    for args in [
        vec!["quality", "--root", missing_root, "--queries", queries],
        vec!["quality", "--root", root, "--queries", "no-such-file.json"],
        vec!["quality", "--root", root, "--queries", malformed],
        vec![
            "quality",
            "--root",
            root,
            "--queries",
            queries,
            "--limit",
            "0",
        ],
        vec![
            "quality",
            "--root",
            root,
            "--queries",
            queries,
            "--limit",
            "101",
        ],
        vec!["quality", "--root", root],
        vec!["quality", "--queries", queries],
        vec!["perf", "--root", missing_root],
        vec!["perf", "--root", root, "--runs", "0"],
        vec!["perf", "--root", root, "--queries", empty],
        vec!["perf"],
        vec![],
    ] {
        let output = sandbox.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    assert!(!sandbox.entries().contains(&".solascope".to_owned()));

    // Without an index, --no-index is the engine's "index missing" error.
    let no_index = [
        "quality",
        "--root",
        root,
        "--queries",
        queries,
        "--no-index",
    ];
    let plain = sandbox.run(&no_index);
    assert_eq!(plain.status.code(), Some(3), "{}", text(&plain.stderr));
    assert!(plain.stdout.is_empty());
    assert!(text(&plain.stderr).contains("error: index not found"));

    let mut with_json = no_index.to_vec();
    with_json.push("--json");
    let json = sandbox.run(&with_json);
    assert_eq!(json.status.code(), Some(3));
    assert!(json.stdout.is_empty());
    let stderr = text(&json.stderr);
    let error: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(error["error"]["exit_code"], 3);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("index not found"));
    assert!(!sandbox.entries().contains(&".solascope".to_owned()));
}
