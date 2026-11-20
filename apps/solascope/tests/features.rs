use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const BINARY: &str = env!("CARGO_BIN_EXE_solascope");
const WAIT: Duration = Duration::from_secs(8);

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "fn initial() {}\n").unwrap();
    root
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("user-config"))
        .env("HOME", root.join("user-home"))
        .env("USERPROFILE", root.join("user-home"))
        .env_remove("RUST_LOG");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SOLASCOPE_") {
            command.env_remove(key);
        }
    }
    command
        .env("SOLASCOPE__PROJECT__ROOT", root)
        .kill_on_drop(true);
    command
}

async fn output(mut command: Command) -> std::process::Output {
    tokio::time::timeout(WAIT, command.output())
        .await
        .unwrap()
        .unwrap()
}

fn success(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn automatic_logs_follow_user_and_explicit_configuration_without_polluting_json() {
    let root = fixture();
    let mut first = command(root.path());
    first.args(["index", "--json"]);
    let indexed = output(first).await;
    assert_eq!(success(&indexed)["files_total"], 1);
    let user_log = root.path().join("user-home/.solascope/logs/solascope.log");
    assert!(std::fs::read_to_string(user_log)
        .unwrap()
        .contains("index committed"));
    let settings = tempfile::tempdir().unwrap();
    let config = settings.path().join("settings.toml");
    std::fs::write(&config, "[log]\nlevel='info'\n").unwrap();
    let mut second = command(root.path());
    second.args(["index", "--json", "--config"]).arg(&config);
    let indexed = output(second).await;
    assert_eq!(success(&indexed)["files_unchanged"], 1);
    assert!(String::from_utf8_lossy(&indexed.stderr).contains("scan complete"));
    let path = settings.path().join("logs/solascope.log");
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(before.contains("scan complete") && before.contains("index committed"));
    assert!(!root.path().join("logs").exists());
    let mut third = command(root.path());
    third.args(["index", "--json", "--config"]).arg(&config);
    success(&output(third).await);
    let after = std::fs::read_to_string(path).unwrap();
    assert!(after.starts_with(&before) && after.len() > before.len());
}

#[tokio::test]
async fn automatic_project_logs_are_excluded_from_index_and_context() {
    let root = fixture();
    std::fs::write(root.path().join(".solascope.toml"), "[log]\nlevel='info'\n").unwrap();
    let mut index = command(root.path());
    index.args(["index", "--json"]);
    assert_eq!(success(&output(index).await)["files_total"], 1);
    let path = root.path().join("logs/solascope.log");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("index committed"));
    let mut context = command(root.path());
    context.args(["context", "logs/solascope.log", "--json"]);
    let failed = output(context).await;
    assert_eq!(failed.status.code(), Some(2));
    assert!(failed.stdout.is_empty());
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("command failed"));
}

#[tokio::test]
async fn model_rate_limit_retries_and_final_errors_are_persisted_without_vendor_bodies() {
    let root = fixture();
    let settings = tempfile::tempdir().unwrap();
    let config = settings.path().join("settings.toml");
    let server = MockServer::start().await;
    std::fs::write(&config, format!("[embedding]\nprovider='ollama'\nmodel='test'\nendpoint='{}'\ndimensions=2\nretries=1\nretry_base_delay_ms=10\nretry_max_delay_ms=100\nrate_limit_cooldown_ms=20\n", server.uri())).unwrap();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": "RateLimitExceeded", "message": "VENDOR_BODY_MUST_NOT_BE_LOGGED"}})))
        .expect(2)
        .mount(&server)
        .await;
    let mut index = command(root.path());
    index.args(["index", "--json", "--config"]).arg(&config);
    let failed = output(index).await;
    assert_eq!(failed.status.code(), Some(4));
    assert!(failed.stdout.is_empty());
    let content = std::fs::read_to_string(settings.path().join("logs/solascope.log")).unwrap();
    assert!(
        content.contains("429") && content.contains("command failed"),
        "{content}"
    );
    assert!(content.contains("existing index is unchanged"), "{content}");
    assert!(!content.contains("VENDOR_BODY_MUST_NOT_BE_LOGGED"));
    assert!(!content.contains(&server.uri()));
}

#[tokio::test]
async fn configured_logs_are_appended_and_never_indexed() {
    let root = fixture();
    std::fs::write(
        root.path().join(".solascope.toml"),
        "[log]\nlevel='info'\noutput='diagnostics.txt'\n",
    )
    .unwrap();
    let mut first = command(root.path());
    first.args(["index", "--json"]);
    let indexed = output(first).await;
    assert_eq!(success(&indexed)["files_total"], 1);
    let log = root.path().join("diagnostics.txt");
    let before = std::fs::read_to_string(&log).unwrap();
    assert!(
        before.contains("scan complete") && before.contains("index committed"),
        "{before}"
    );
    assert!(!before.contains('\u{1b}'));
    let mut second = command(root.path());
    second.args(["index", "--json"]);
    let indexed = output(second).await;
    assert_eq!(success(&indexed)["files_unchanged"], 1);
    let after = std::fs::read_to_string(&log).unwrap();
    assert!(after.starts_with(&before) && after.len() > before.len());
}

#[tokio::test]
async fn the_configured_log_level_is_respected_and_unrelated_files_are_safe() {
    let root = fixture();
    std::fs::write(
        root.path().join(".solascope.toml"),
        "[log]\nlevel='off'\noutput='quiet.log'\n",
    )
    .unwrap();
    let mut index = command(root.path());
    index.args(["index", "--json"]);
    success(&output(index).await);
    let path = root.path().join("quiet.log");
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("scan complete"));
    std::fs::write(&path, "unrelated file").unwrap();
    let mut status = command(root.path());
    status.args(["status", "--json"]);
    let failed = output(status).await;
    assert_eq!(failed.status.code(), Some(2));
    assert!(failed.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "unrelated file");
}

#[tokio::test]
async fn startup_probe_checks_supplied_models_and_doctor_reports_failures() {
    let root = fixture();
    let settings = tempfile::tempdir().unwrap();
    let path_config = settings.path().join("providers.toml");
    let server = MockServer::start().await;
    std::fs::write(&path_config, format!(
        "[embedding]\nprovider='ollama'\nmodel='test'\nendpoint='{}'\ndimensions=2\ntimeout_secs=1\nretries=0\n", server.uri()
    )).unwrap();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": [[1.0, 0.25]]})),
        )
        .mount(&server)
        .await;
    let mut init = command(root.path());
    init.args(["init", "--probe", "--json", "--config"])
        .arg(&path_config);
    let checked = output(init).await;
    let report = success(&checked);
    assert_eq!(report["healthy"], true);
    assert_eq!(report["checks"][0]["status"], "ok");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["input"], json!(["SolaScope capability probe"]));
    assert!(!requests[0].headers.contains_key("authorization"));
    // The generated user config still defaults to models disabled.
    assert!(!root.path().join(".solascope.toml").exists());
    let created =
        std::fs::read_to_string(root.path().join("user-home/.solascope/.solascope.toml")).unwrap();
    assert!(created.contains("provider = \"disabled\""));
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mut doctor = command(root.path());
    doctor
        .args(["doctor", "--models", "--json", "--config"])
        .arg(&path_config);
    let failed = output(doctor).await;
    assert_eq!(failed.status.code(), Some(4));
    let report: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "embedding" && check["status"] == "error"));
}

#[tokio::test]
async fn no_watch_overrides_the_configuration_and_flags_conflict() {
    let root = fixture();
    std::fs::write(root.path().join(".solascope.toml"), "[index]\nwatch=true\n").unwrap();
    let mut once = command(root.path());
    once.args(["index", "--no-watch", "--json"]);
    assert_eq!(success(&output(once).await)["files_updated"], 1);
    let mut conflicting = command(root.path());
    conflicting.args(["index", "--watch", "--no-watch"]);
    assert_eq!(output(conflicting).await.status.code(), Some(2));
}

#[tokio::test]
async fn index_watch_runs_from_both_flag_and_configuration() {
    for from_config in [false, true] {
        let root = fixture();
        if from_config {
            std::fs::write(
                root.path().join(".solascope.toml"),
                "[index]\nwatch=true\ndebounce_ms=20\n",
            )
            .unwrap();
        }
        let mut watching = command(root.path());
        watching.args(["index", "--json"]);
        if !from_config {
            watching.arg("--watch");
        }
        watching.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = watching.spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let report: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(report["files_updated"], 1);
        std::fs::write(root.path().join("lib.rs"), "fn modified_live() {}\n").unwrap();
        tokio::time::timeout(WAIT, async {
            loop {
                let mut find = command(root.path());
                find.args(["symbol", "modified_live", "--json"]);
                let found = output(find).await;
                if found.status.success() {
                    let value = success(&found);
                    if value["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|result| result["stale"] == false)
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        #[cfg(unix)]
        {
            let killed = Command::new("kill")
                .arg("-INT")
                .arg(child.id().unwrap().to_string())
                .status()
                .await
                .unwrap();
            assert!(killed.success());
            assert!(tokio::time::timeout(WAIT, child.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
        }
        #[cfg(not(unix))]
        child.kill().await.unwrap();
        assert!(tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn mcp_startup_probe_never_pollutes_stdout() {
    let root = fixture();
    let mut server = command(root.path());
    server.args(["mcp", "--probe"]).stdin(Stdio::null());
    let closed = output(server).await;
    assert!(closed.status.success());
    assert!(closed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&closed.stderr).contains("embedding: ok"));
}

#[tokio::test]
async fn cloud_credentials_are_read_from_dotenv_beside_the_explicit_config() {
    let root = fixture();
    let settings = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let config = settings.path().join("cloud.toml");
    let embedding_key = "mock-dotenv-embedding";
    let rerank_key = "mock-dotenv-rerank";
    std::fs::write(&config, format!(
        "[privacy]\nremote_opt_in=true\n[embedding]\nprovider='dashscope'\nmodel='text-embedding-v4'\nendpoint='{}'\ndimensions=2\napi_key_env='SOLASCOPE_DOTENV_EMBED_ONLY_KEY'\n[rerank]\nprovider='siliconflow'\nmodel='BAAI/bge-reranker-v2-m3'\nendpoint='{}'\napi_key_env='SOLASCOPE_DOTENV_RERANK_ONLY_KEY'\ntop_n=2\n",
        server.uri(), server.uri(),
    )).unwrap();
    std::fs::write(settings.path().join(".env"), format!(
        "SOLASCOPE_DOTENV_EMBED_ONLY_KEY={embedding_key}\nSOLASCOPE_DOTENV_RERANK_ONLY_KEY={rerank_key}\n"
    )).unwrap();
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/services/embeddings/text-embedding/text-embedding",
        ))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {embedding_key}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": {"embeddings": [{"text_index": 0, "embedding": [1.0, 0.25]}]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {rerank_key}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"index": 0, "relevance_score": 0.8}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let mut doctor = command(root.path());
    doctor
        .args(["doctor", "--models", "--json", "--config"])
        .arg(&config);
    let report = output(doctor).await;
    assert!(
        report.status.success(),
        "{}{}",
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    assert_eq!(success(&report)["healthy"], true);
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    assert!(!rendered.contains(embedding_key));
    assert!(!rendered.contains(rerank_key));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
