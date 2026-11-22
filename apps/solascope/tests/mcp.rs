use std::{path::Path, process::Stdio, time::Duration};

use rmcp::{model::CallToolRequestParams, transport::TokioChildProcess, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const BINARY: &str = env!("CARGO_BIN_EXE_solascope");
const WAIT: Duration = Duration::from_secs(10);

fn command(root: &Path) -> Command {
    let mut command = Command::new(BINARY);
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
    command.env("SOLASCOPE__PROJECT__ROOT", root);
    command.kill_on_drop(true);
    command
}

async fn fixture(index: bool) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    // Stop configuration discovery before unrelated settings in the temp parent.
    std::fs::create_dir(directory.path().join(".git")).unwrap();
    std::fs::write(
        directory.path().join("fixture.rs"),
        "pub fn mcp_fixture_symbol() -> usize {\n    7\n}\n",
    )
    .unwrap();
    if index {
        let output = timeout(
            WAIT,
            command(directory.path()).args(["index", "--json"]).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    directory
}

struct NativeClient {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl NativeClient {
    async fn start(root: &Path) -> Self {
        Self::start_with_args(root, &[]).await
    }

    async fn start_with_args(root: &Path, args: &[&str]) -> Self {
        let mut child = command(root)
            .arg("mcp")
            .env_remove("SOLASCOPE__PROJECT__ROOT")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut client = Self {
            child,
            input,
            output,
            next_id: 1,
        };
        let response = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-03-26", "capabilities": {},
                    "clientInfo": { "name": "solascope-native-test", "version": "1" }
                }),
            )
            .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["serverInfo"]["name"], "solascope");
        assert!(response["result"]["capabilities"]["tools"].is_object());
        client
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        client
    }

    async fn send(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        timeout(WAIT, self.input.as_mut().unwrap().write_all(&bytes))
            .await
            .unwrap()
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let line = timeout(WAIT, self.output.next_line())
                .await
                .unwrap()
                .unwrap()
                .expect("server closed stdout early");
            let response: Value =
                serde_json::from_str(&line).expect("every stdout line must be JSON-RPC");
            assert_eq!(response["jsonrpc"], "2.0");
            if response["id"] == id {
                return response;
            }
            assert!(
                response.get("id").is_none(),
                "unexpected response: {response}"
            );
        }
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    async fn close(mut self) {
        drop(self.input.take());
        let output = timeout(WAIT, self.child.wait_with_output())
            .await
            .expect("server must exit after stdio EOF")
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn native_json_rpc_initialization_schema_calls_errors_and_eof() {
    let root = fixture(true).await;
    let mut client = NativeClient::start(root.path()).await;
    let list = client.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "find_symbol",
            "get_code_context",
            "index",
            "index_status",
            "reindex",
            "search_code"
        ]
    );
    for tool in tools {
        let writes_index = matches!(tool["name"].as_str(), Some("index" | "reindex"));
        assert_eq!(tool["annotations"]["readOnlyHint"], !writes_index, "{tool}");
        assert_eq!(
            tool["annotations"]["destructiveHint"], writes_index,
            "{tool}"
        );
        assert_eq!(
            tool["annotations"]["openWorldHint"],
            writes_index || tool["name"] == "search_code",
            "{tool}"
        );
        if writes_index {
            assert_ne!(tool["annotations"]["idempotentHint"], true, "{tool}");
            assert!(tool["description"]
                .as_str()
                .unwrap()
                .contains("authorization"));
            assert!(tool["description"].as_str().unwrap().contains("charges"));
            assert!(tool["outputSchema"]["properties"].get("rebuilt").is_some());
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert_eq!(
                tool["inputSchema"]["properties"].as_object().unwrap().len(),
                1
            );
        }
        assert_eq!(tool["outputSchema"]["type"], "object", "{tool}");
        assert!(
            tool["inputSchema"]["properties"].get("root").is_some(),
            "{tool}"
        );
        assert!(
            !tool["inputSchema"]["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!("root"))),
            "{tool}"
        );
        for (name, property) in tool["inputSchema"]["properties"]
            .as_object()
            .into_iter()
            .flatten()
        {
            assert!(
                property["description"].is_string(),
                "{} parameter {name} needs a description",
                tool["name"]
            );
        }
    }
    let search = tools
        .iter()
        .find(|tool| tool["name"] == "search_code")
        .unwrap();
    let schema = &search["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("query")));
    assert!(schema["properties"].get("root").is_some());
    assert!(!schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("root")));
    let result = client.call("search_code", json!({ "query": "mcp_fixture_symbol", "mode": "lexical", "limit": 1, "rerank": false })).await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    let structured = &result["result"]["structuredContent"];
    assert_eq!(structured["results"][0]["relative_path"], "fixture.rs");
    assert_eq!(structured["results"][0]["stale"], false);
    assert_eq!(
        structured["results"][0]["match_sources"],
        json!(["lexical"])
    );
    for field in search["outputSchema"]["required"].as_array().unwrap() {
        assert!(structured.get(field.as_str().unwrap()).is_some());
    }
    let text: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, structured);
    let symbol = client
        .call(
            "find_symbol",
            json!({ "name": "mcp_fixture_symbol", "kind": "function" }),
        )
        .await;
    assert_eq!(symbol["result"]["isError"], false, "{symbol}");
    assert!(!symbol["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    let context = client
        .call(
            "get_code_context",
            json!({ "path": "fixture.rs", "start_line": 1, "end_line": 3 }),
        )
        .await;
    assert_eq!(
        context["result"]["structuredContent"]["lines"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    for arguments in [
        json!({ "path": "../outside.rs", "line": 1 }),
        json!({ "path": "/etc/passwd", "line": 1 }),
        json!({ "path": "fixture.rs", "start_line": 1, "end_line": 201 }),
        json!({ "path": "fixture.rs", "start_line": 2 }),
        json!({ "path": "fixture.rs", "line": 0 }),
    ] {
        let result = client.call("get_code_context", arguments).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert!(result["result"]["structuredContent"]["error"]["message"].is_string());
    }
    for arguments in [
        json!({ "query": "x", "limit": 0 }),
        json!({ "query": "x", "limit": 101 }),
        json!({ "query": " " }),
        json!({ "query": "x".repeat(4097) }),
    ] {
        let result = client.call("search_code", arguments).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
    }
    for arguments in [
        json!({ "query": 42 }),
        json!({ "query": "x", "mode": "unknown" }),
        json!({ "query": "x", "index_dir": "/tmp" }),
    ] {
        let result = client.call("search_code", arguments).await;
        // rmcp reports schema/deserialization failures as standard tool errors.
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert!(result["result"]["content"][0]["text"].is_string());
    }
    let model_error = client
        .call("search_code", json!({ "query": "x", "mode": "semantic" }))
        .await;
    assert_eq!(model_error["result"]["isError"], true, "{model_error}");
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    client.close().await;
}

#[tokio::test]
async fn index_creates_updates_and_reindex_rebuilds_without_a_restart() {
    let root = fixture(false).await;
    let mut client = NativeClient::start(root.path()).await;
    assert!(!root.path().join(".solascope").exists());

    let list = client.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    for tool_name in ["index", "reindex"] {
        let created = client.call(tool_name, json!({})).await;
        assert_eq!(created["result"]["isError"], false, "{created}");
        let report = &created["result"]["structuredContent"];
        assert_eq!(report["rebuilt"], tool_name == "reindex");
        assert_eq!(report["files_total"], 1);
        assert_eq!(report["files_updated"], 1);
        assert!(report["chunks"].as_u64().unwrap() > 0);
        let tool = tools.iter().find(|tool| tool["name"] == tool_name).unwrap();
        for field in tool["outputSchema"]["required"].as_array().unwrap() {
            assert!(report.get(field.as_str().unwrap()).is_some(), "{report}");
        }
        let text: Value =
            serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(&text, report);
    }
    assert!(root.path().join(".solascope/index.sqlite3").exists());
    let before = client.call("index_status", json!({})).await;
    let generation = before["result"]["structuredContent"]["index_version"]
        .as_u64()
        .unwrap();
    let unchanged = client.call("index", json!({})).await;
    assert_eq!(unchanged["result"]["isError"], false, "{unchanged}");
    assert_eq!(unchanged["result"]["structuredContent"]["files_updated"], 0);
    assert_eq!(
        unchanged["result"]["structuredContent"]["files_unchanged"],
        1
    );
    assert_eq!(
        unchanged["result"]["structuredContent"]["index_version"],
        generation
    );

    std::fs::write(
        root.path().join("fixture.rs"),
        "pub fn updated_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(root.path().join("added.rs"), "pub fn added_symbol() {}\n").unwrap();
    let updated = client.call("index", json!({"root": null})).await;
    assert_eq!(updated["result"]["isError"], false, "{updated}");
    assert_eq!(updated["result"]["structuredContent"]["files_updated"], 2);
    assert_eq!(updated["result"]["structuredContent"]["files_total"], 2);
    assert!(
        updated["result"]["structuredContent"]["index_version"]
            .as_u64()
            .unwrap()
            > generation
    );
    let result = client
        .call("find_symbol", json!({"name": "updated_symbol"}))
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["results"][0]["relative_path"],
        "fixture.rs"
    );
    let removed = client
        .call("find_symbol", json!({"name": "mcp_fixture_symbol"}))
        .await;
    assert!(removed["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());

    std::fs::remove_file(root.path().join("added.rs")).unwrap();
    let deleted = client.call("index", json!({})).await;
    assert_eq!(deleted["result"]["isError"], false, "{deleted}");
    assert_eq!(deleted["result"]["structuredContent"]["files_deleted"], 1);
    assert_eq!(deleted["result"]["structuredContent"]["files_unchanged"], 1);
    let generation = deleted["result"]["structuredContent"]["index_version"]
        .as_u64()
        .unwrap();
    let rebuilt = client.call("reindex", json!({})).await;
    assert_eq!(rebuilt["result"]["isError"], false, "{rebuilt}");
    assert_eq!(rebuilt["result"]["structuredContent"]["rebuilt"], true);
    assert_eq!(rebuilt["result"]["structuredContent"]["files_updated"], 1);
    assert_eq!(rebuilt["result"]["structuredContent"]["files_unchanged"], 0);
    assert!(
        rebuilt["result"]["structuredContent"]["index_version"]
            .as_u64()
            .unwrap()
            > generation
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("fixture.rs")).unwrap(),
        "pub fn updated_symbol() {}\n"
    );
    let missing = client
        .call("find_symbol", json!({"name": "added_symbol"}))
        .await;
    assert!(missing["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    client.close().await;
}

#[tokio::test]
async fn maintenance_roots_use_project_settings_without_changing_the_default() {
    let first = fixture(false).await;
    let second = fixture(false).await;
    std::fs::write(
        second.path().join(".solascope.toml"),
        "[project]\nindex_dir='custom-index'\nexclude=['private.rs']\n[index]\nwatch=true\n[parser]\nmax_chunk_tokens=256\n",
    )
    .unwrap();
    std::fs::write(
        second.path().join("private.rs"),
        "pub fn secret_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(
        second.path().join(".env"),
        "MCP_PRIVATE_FIXTURE=private-value\n",
    )
    .unwrap();
    let nested = first.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::create_dir(nested.join(".git")).unwrap();
    std::fs::write(nested.join("nested.rs"), "pub fn nested_symbol() {}\n").unwrap();
    let mut client = NativeClient::start(first.path()).await;
    let created = client.call("reindex", json!({"root": second.path()})).await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    assert_eq!(created["result"]["structuredContent"]["rebuilt"], true);
    assert_eq!(created["result"]["structuredContent"]["files_total"], 1);
    assert!(second.path().join("custom-index/index.sqlite3").exists());
    assert!(!second.path().join(".solascope").exists());
    assert!(!first.path().join(".solascope").exists());
    let denied = client
        .call(
            "find_symbol",
            json!({"root": second.path(), "name": "secret_symbol"}),
        )
        .await;
    assert!(denied["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    let default = client.call("index_status", json!({})).await;
    assert_eq!(
        default["result"]["structuredContent"]["error"]["exit_code"],
        3
    );

    let relative = client.call("index", json!({"root": "nested"})).await;
    assert_eq!(relative["result"]["isError"], false, "{relative}");
    assert_eq!(relative["result"]["structuredContent"]["files_total"], 1);
    assert!(nested.join(".solascope/index.sqlite3").exists());
    assert!(!first.path().join(".solascope").exists());
    let created = client.call("index", json!({})).await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    let default = client.call("index_status", json!({})).await;
    assert_eq!(
        default["result"]["structuredContent"]["root"],
        json!(first.path().canonicalize().unwrap())
    );
    let second_status = client
        .call("index_status", json!({"root": second.path()}))
        .await;
    assert_eq!(
        second_status["result"]["structuredContent"]["root"],
        json!(second.path().canonicalize().unwrap())
    );
    client.close().await;
}

#[tokio::test]
async fn maintenance_rejects_invalid_arguments_and_writer_conflicts() {
    let root = fixture(true).await;
    let mut client = NativeClient::start(root.path()).await;
    for tool in ["index", "reindex"] {
        for root in [
            json!(""),
            json!(" "),
            json!("bad\u{0000}root"),
            json!("x".repeat(4097)),
            json!("missing"),
            json!("fixture.rs"),
        ] {
            let result = client.call(tool, json!({"root": root})).await;
            assert_eq!(result["result"]["isError"], true, "{result}");
            assert_eq!(
                result["result"]["structuredContent"]["error"]["exit_code"], 2,
                "{result}"
            );
        }
        for arguments in [
            json!({"root": 42}),
            json!({"watch": true}),
            json!({"rebuild": true}),
            json!({"index_dir": "elsewhere"}),
        ] {
            let result = client.call(tool, arguments).await;
            assert_eq!(result["result"]["isError"], true, "{result}");
        }
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join(".solascope/writer.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let before = client.call("index_status", json!({})).await;
    for tool in ["index", "reindex"] {
        let result = client.call(tool, json!({})).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["error"]["exit_code"], 5,
            "{result}"
        );
    }
    let during = client.call("index_status", json!({})).await;
    assert_eq!(
        during["result"]["structuredContent"]["index_version"],
        before["result"]["structuredContent"]["index_version"]
    );
    drop(lock);
    let result = client.call("reindex", json!({})).await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert!(!root.path().join("elsewhere").exists());
    client.close().await;
}

#[tokio::test]
async fn index_repairs_outdated_chunking_without_implicit_query_writes() {
    let root = fixture(true).await;
    let settings = tempfile::tempdir().unwrap();
    let config = settings.path().join("parser.toml");
    std::fs::write(&config, "[parser]\nmax_chunk_tokens=256\n").unwrap();
    let mut client =
        NativeClient::start_with_args(root.path(), &["--config", config.to_str().unwrap()]).await;
    let outdated = client.call("index_status", json!({})).await;
    assert_eq!(
        outdated["result"]["structuredContent"]["error"]["exit_code"],
        3
    );
    let repaired = client.call("index", json!({})).await;
    assert_eq!(repaired["result"]["isError"], false, "{repaired}");
    assert_eq!(repaired["result"]["structuredContent"]["files_updated"], 1);
    assert!(!repaired["result"]["structuredContent"]["warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    let source = client
        .call("find_symbol", json!({"name": "mcp_fixture_symbol"}))
        .await;
    assert_eq!(
        source["result"]["structuredContent"]["results"][0]["relative_path"],
        "fixture.rs"
    );
    client.close().await;
}

#[tokio::test]
async fn reindex_recovers_a_fingerprint_change_without_deleting_the_previous_index() {
    let root = fixture(true).await;
    let server = MockServer::start().await;
    std::fs::write(root.path().join(".solascope.toml"), format!(
        "[embedding]\nprovider='ollama'\nmodel='test-model'\nendpoint='{}'\ndimensions=2\nretries=0\n", server.uri(),
    )).unwrap();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let embeddings: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": embeddings}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let mut client = NativeClient::start(root.path()).await;
    let before = client.call("index_status", json!({})).await;
    let incompatible = client.call("index", json!({})).await;
    assert_eq!(incompatible["result"]["isError"], true, "{incompatible}");
    assert_eq!(
        incompatible["result"]["structuredContent"]["error"]["exit_code"],
        4
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let preserved = client.call("index_status", json!({})).await;
    assert_eq!(
        preserved["result"]["structuredContent"]["index_version"],
        before["result"]["structuredContent"]["index_version"]
    );
    let old = client
        .call(
            "search_code",
            json!({"query": "mcp_fixture_symbol", "mode": "lexical", "rerank": false}),
        )
        .await;
    assert_eq!(old["result"]["isError"], false, "{old}");
    let rebuilt = client.call("reindex", json!({})).await;
    assert_eq!(rebuilt["result"]["isError"], false, "{rebuilt}");
    assert_eq!(rebuilt["result"]["structuredContent"]["rebuilt"], true);
    let status = client.call("index_status", json!({})).await;
    assert_eq!(
        status["result"]["structuredContent"]["embedding_fingerprint"]["model"],
        "test-model"
    );
    assert!(
        status["result"]["structuredContent"]["vectors"]
            .as_u64()
            .unwrap()
            > 0
    );
    client.close().await;
}

#[tokio::test]
async fn reindex_recomputes_relocated_vectors_and_reuses_exact_cached_vectors() {
    let root = fixture(false).await;
    let server = MockServer::start().await;
    std::fs::write(
        root.path().join(".solascope.toml"),
        format!("[embedding]\nprovider='ollama'\nmodel='test-model'\nendpoint='{}'\ndimensions=2\nretries=0\n", server.uri()),
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let embeddings: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": embeddings}))
        })
        .mount(&server)
        .await;
    let mut client = NativeClient::start(root.path()).await;
    assert!(server.received_requests().await.unwrap().is_empty());
    let created = client.call("index", json!({})).await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    let embeddings = created["result"]["structuredContent"]["embedded_chunks"]
        .as_u64()
        .unwrap();
    assert!(embeddings > 0);
    let requests = server.received_requests().await.unwrap().len();
    std::fs::rename(
        root.path().join("fixture.rs"),
        root.path().join("renamed.rs"),
    )
    .unwrap();
    let moved = client.call("index", json!({})).await;
    assert_eq!(moved["result"]["isError"], false, "{moved}");
    assert_eq!(moved["result"]["structuredContent"]["files_deleted"], 1);
    assert_eq!(moved["result"]["structuredContent"]["embedded_chunks"], 0);
    assert_eq!(
        moved["result"]["structuredContent"]["relocated_embeddings"],
        embeddings
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    let status = client.call("index_status", json!({})).await;
    assert_eq!(
        status["result"]["structuredContent"]["relocated_vectors"],
        embeddings
    );

    let rebuilt = client.call("reindex", json!({})).await;
    assert_eq!(rebuilt["result"]["isError"], false, "{rebuilt}");
    assert_eq!(rebuilt["result"]["structuredContent"]["rebuilt"], true);
    assert_eq!(
        rebuilt["result"]["structuredContent"]["embedded_chunks"],
        embeddings
    );
    assert_eq!(
        rebuilt["result"]["structuredContent"]["relocated_embeddings"],
        0
    );
    let status = client.call("index_status", json!({})).await;
    assert_eq!(
        status["result"]["structuredContent"]["relocated_vectors"],
        0
    );
    let requests = server.received_requests().await.unwrap().len();
    let cached = client.call("reindex", json!({})).await;
    assert_eq!(cached["result"]["isError"], false, "{cached}");
    assert_eq!(cached["result"]["structuredContent"]["embedded_chunks"], 0);
    assert_eq!(
        cached["result"]["structuredContent"]["cache_hits"],
        embeddings
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    client.close().await;
}

#[tokio::test]
async fn maintenance_preserves_remote_opt_in_and_project_dotenv_isolation() {
    let first = fixture(false).await;
    let second = fixture(false).await;
    let denied = fixture(false).await;
    let server = MockServer::start().await;
    for (project, key, opted_in) in [
        (first.path(), "first-index-key", true),
        (second.path(), "second-index-key", true),
        (denied.path(), "denied-index-key", false),
    ] {
        std::fs::write(project.join(".solascope.toml"), format!(
            "[privacy]\nremote_opt_in={opted_in}\n[project]\nexclude=['private.rs']\n[embedding]\nprovider='remote'\nmodel='test-model'\nendpoint='{}'\ndimensions=2\napi_key_env='SOLASCOPE_MCP_INDEX_KEY'\nretries=0\n",
            server.uri(),
        )).unwrap();
        std::fs::write(
            project.join(".env"),
            format!("SOLASCOPE_MCP_INDEX_KEY={key}\n"),
        )
        .unwrap();
        std::fs::write(
            project.join("private.rs"),
            "pub fn private_index_secret() {}\n",
        )
        .unwrap();
        if opted_in {
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .and(wiremock::matchers::header(
                    "authorization",
                    format!("Bearer {key}"),
                ))
                .respond_with(|request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(!body.to_string().contains("private_index_secret"));
                    assert!(!body.to_string().contains("index-key"));
                    let data: Vec<_> = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(index, _)| json!({"index": index, "embedding": [1.0, 0.25]}))
                        .collect();
                    ResponseTemplate::new(200).set_body_json(json!({"data": data}))
                })
                .expect(1)
                .mount(&server)
                .await;
        }
    }
    let mut client = NativeClient::start(first.path()).await;
    assert!(server.received_requests().await.unwrap().is_empty());
    for tool in ["index", "reindex"] {
        let result = client.call(tool, json!({"root": denied.path()})).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["error"]["exit_code"], 2,
            "{result}"
        );
    }
    assert!(!denied.path().join(".solascope").exists());
    assert!(server.received_requests().await.unwrap().is_empty());
    for (tool, root) in [("index", Value::Null), ("reindex", json!(second.path()))] {
        let result = client.call(tool, json!({"root": root})).await;
        assert_eq!(result["result"]["isError"], false, "{result}");
        assert_eq!(result["result"]["structuredContent"]["files_total"], 1);
        assert!(!result.to_string().contains("index-key"));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    client.close().await;
}

#[tokio::test]
async fn cancelled_maintenance_preserves_generation_cleans_jobs_and_releases_the_writer() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let root = fixture(false).await;
    let server = MockServer::start().await;
    let delay = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    std::fs::write(
        root.path().join(".solascope.toml"),
        format!("[embedding]\nprovider='ollama'\nmodel='test-model'\nendpoint='{}'\ndimensions=2\nretries=0\ntimeout_secs=60\n", server.uri()),
    )
    .unwrap();
    let flag = delay.clone();
    let notified = started.clone();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let embeddings: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            let response =
                ResponseTemplate::new(200).set_body_json(json!({"embeddings": embeddings}));
            if flag.load(Ordering::SeqCst) {
                notified.notify_one();
                response.set_delay(Duration::from_secs(60))
            } else {
                response
            }
        })
        .mount(&server)
        .await;
    let mut client = NativeClient::start(root.path()).await;
    let created = client.call("index", json!({})).await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    for tool in ["index", "reindex"] {
        let before = client.call("index_status", json!({})).await;
        let generation = before["result"]["structuredContent"]["index_version"].clone();
        let symbol = format!("cancelled_{tool}_symbol");
        std::fs::write(
            root.path().join("fixture.rs"),
            format!("pub fn {symbol}() {{}}\n"),
        )
        .unwrap();
        delay.store(true, Ordering::SeqCst);
        let id = client.next_id;
        client.next_id += 1;
        client.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": {}}})).await;
        timeout(WAIT, started.notified())
            .await
            .expect("index must reach the delayed model request");
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "cancel indexing"}})).await;
        // rmcp suppresses responses for cancelled request IDs. Wait through read-only
        // status calls until the engine has recorded the failed job and dropped its lock.
        timeout(WAIT, async {
            loop {
                let status = client.call("index_status", json!({})).await;
                assert_eq!(status["result"]["isError"], false, "{status}");
                assert_eq!(
                    status["result"]["structuredContent"]["index_version"],
                    generation
                );
                if status["result"]["structuredContent"]["pending_jobs"] == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled indexing must clean up its job");
        let unpublished = client.call("find_symbol", json!({"name": &symbol})).await;
        // Spelling correction can return a previously published, similarly named symbol.
        assert!(unpublished["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|result| result["symbol"] != symbol));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join(".solascope/writer.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock)
            .expect("cancelled indexing must release its writer lock");
        drop(lock);
        delay.store(false, Ordering::SeqCst);
        let retried = client.call(tool, json!({})).await;
        assert_eq!(retried["result"]["isError"], false, "{retried}");
        assert!(
            retried["result"]["structuredContent"]["index_version"]
                .as_u64()
                .unwrap()
                > generation.as_u64().unwrap()
        );
    }
    client.close().await;
}

#[tokio::test]
async fn tool_roots_select_projects_without_changing_the_default_directory() {
    let first = fixture(true).await;
    let second = fixture(false).await;
    std::fs::write(
        second.path().join("fixture.rs"),
        "pub fn mcp_fixture_symbol() -> usize { 42 }\n",
    )
    .unwrap();
    let indexed = command(second.path())
        .args(["index", "--json"])
        .output()
        .await
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let mut client = NativeClient::start(first.path()).await;
    for (root, project, source) in [
        (Value::Null, first.path(), "7"),
        (json!(second.path()), second.path(), "42"),
        (json!("."), first.path(), "7"),
        (json!(second.path()), second.path(), "42"),
        (Value::Null, first.path(), "7"),
    ] {
        let status = client.call("index_status", json!({"root": root})).await;
        assert_eq!(status["result"]["isError"], false, "{status}");
        let resolved = std::fs::canonicalize(project).unwrap();
        let status = &status["result"]["structuredContent"];
        assert_eq!(status["root"], json!(resolved));
        assert_eq!(status["index_dir"], json!(resolved.join(".solascope")));
        for (tool, key) in [("search_code", "query"), ("find_symbol", "name")] {
            let mut arguments = json!({"root": root, key: "mcp_fixture_symbol"});
            if tool == "search_code" {
                arguments["mode"] = json!("lexical");
            }
            let result = client.call(tool, arguments).await;
            assert_eq!(result["result"]["isError"], false, "{result}");
            let hit = &result["result"]["structuredContent"]["results"][0];
            assert_eq!(hit["project_id"], status["project_id"]);
            assert!(
                hit["snippet"].as_str().unwrap().contains(source),
                "{result}"
            );
        }
        let context = client
            .call(
                "get_code_context",
                json!({"root": root, "path": "fixture.rs", "line": 1}),
            )
            .await;
        assert_eq!(context["result"]["isError"], false, "{context}");
        assert!(
            context["result"]["structuredContent"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line["text"].as_str().unwrap().contains(source)),
            "{context}"
        );
    }
    let default = client.call("index_status", json!({})).await;
    assert_eq!(
        default["result"]["structuredContent"]["root"],
        json!(std::fs::canonicalize(first.path()).unwrap())
    );
    client.close().await;
}

#[tokio::test]
async fn project_configuration_controls_index_location_search_defaults_and_privacy() {
    let first = fixture(true).await;
    let second = fixture(false).await;
    std::fs::write(second.path().join(".solascope.toml"), "[project]\nindex_dir='custom-index'\nexclude=['private.rs']\n[search]\nmode='lexical'\nlimit=1\n[parser]\nmax_chunk_tokens=256\n").unwrap();
    std::fs::write(
        second.path().join("fixture.rs"),
        "pub fn mcp_fixture_symbol() {}\npub fn mcp_fixture_symbol_extra() {}\n",
    )
    .unwrap();
    std::fs::write(
        second.path().join("private.rs"),
        "pub fn mcp_private_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(
        first.path().join("private.rs"),
        "pub fn mcp_visible_symbol() {}\n",
    )
    .unwrap();
    let indexed = command(second.path())
        .args(["index", "--json"])
        .output()
        .await
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let mut client = NativeClient::start(first.path()).await;
    let status = client
        .call("index_status", json!({"root": second.path()}))
        .await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    assert_eq!(
        status["result"]["structuredContent"]["index_dir"],
        json!(std::fs::canonicalize(second.path())
            .unwrap()
            .join("custom-index"))
    );
    assert!(!second.path().join(".solascope/index.sqlite3").exists());
    for (tool, key) in [("search_code", "query"), ("find_symbol", "name")] {
        let result = client
            .call(
                tool,
                json!({"root": second.path(), key: "mcp_fixture_symbol"}),
            )
            .await;
        assert_eq!(result["result"]["isError"], false, "{result}");
        assert_eq!(result["result"]["structuredContent"]["mode"], "lexical");
        assert_eq!(
            result["result"]["structuredContent"]["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    let denied = client
        .call(
            "get_code_context",
            json!({"root": second.path(), "path": "private.rs", "line": 1}),
        )
        .await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    let visible = client
        .call("get_code_context", json!({"path": "private.rs", "line": 1}))
        .await;
    assert_eq!(visible["result"]["isError"], false, "{visible}");
    let overridden = client.call("search_code", json!({"root": second.path(), "query": "mcp_fixture_symbol", "limit": 10, "mode": "hybrid", "rerank": false})).await;
    assert_eq!(overridden["result"]["isError"], false, "{overridden}");
    assert_eq!(overridden["result"]["structuredContent"]["mode"], "hybrid");
    assert_eq!(
        overridden["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    client.close().await;

    let mut client = NativeClient::start_with_args(
        first.path(),
        &["--limit", "10", "--mode", "hybrid", "--no-rerank"],
    )
    .await;
    let result = client
        .call(
            "search_code",
            json!({"root": second.path(), "query": "mcp_fixture_symbol"}),
        )
        .await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(result["result"]["structuredContent"]["mode"], "hybrid");
    assert_eq!(
        result["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    client.close().await;
}

#[tokio::test]
async fn startup_root_is_optional_and_relative_tool_roots_use_the_working_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let project = workspace.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("fixture.rs"),
        "pub fn mcp_fixture_symbol() {}\n",
    )
    .unwrap();
    let indexed = command(&project)
        .args(["index", "--json"])
        .output()
        .await
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let mut client = NativeClient::start_with_args(workspace.path(), &["--root", "project"]).await;
    for arguments in [json!({}), json!({"root": "project"})] {
        let status = client.call("index_status", arguments).await;
        assert_eq!(status["result"]["isError"], false, "{status}");
        assert_eq!(
            status["result"]["structuredContent"]["root"],
            json!(std::fs::canonicalize(&project).unwrap())
        );
    }
    let absent = client.call("index_status", json!({"root": "."})).await;
    assert_eq!(
        absent["result"]["structuredContent"]["error"]["exit_code"], 3,
        "{absent}"
    );
    assert!(!workspace.path().join(".solascope/index.sqlite3").exists());
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    client.close().await;
}

#[tokio::test]
async fn selected_projects_keep_their_own_dotenv_credentials() {
    let first = fixture(true).await;
    let second = fixture(true).await;
    let server = MockServer::start().await;
    for (project, key) in [
        (first.path(), "first-project-key"),
        (second.path(), "second-project-key"),
    ] {
        std::fs::write(project.join(".solascope.toml"), format!(
            "[privacy]\nremote_opt_in=true\n[search]\nmode='lexical'\n[rerank]\nprovider='siliconflow'\nmodel='test-reranker'\nendpoint='{}'\napi_key_env='SOLASCOPE_MCP_PROJECT_KEY'\nfail_open=false\n",
            server.uri()
        )).unwrap();
        std::fs::write(
            project.join(".env"),
            format!("SOLASCOPE_MCP_PROJECT_KEY={key}\n"),
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/rerank"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {key}"),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"results": [{"index": 0, "relevance_score": 0.9}]})),
            )
            .expect(2)
            .mount(&server)
            .await;
    }
    let mut client = NativeClient::start(first.path()).await;
    for root in [
        json!(second.path()),
        Value::Null,
        json!(second.path()),
        Value::Null,
    ] {
        let result = client
            .call(
                "search_code",
                json!({"root": root, "query": "mcp_fixture_symbol"}),
            )
            .await;
        assert_eq!(result["result"]["isError"], false, "{result}");
        let text = result.to_string();
        assert!(!text.contains("first-project-key") && !text.contains("second-project-key"));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    client.close().await;
}

#[tokio::test]
async fn explicit_configuration_is_preserved_when_selecting_a_different_project() {
    let first = fixture(true).await;
    let second = fixture(true).await;
    // An explicit configuration must bypass the selected project's local file.
    std::fs::write(
        second.path().join(".solascope.toml"),
        "[unknown_section]\nvalue=true\n",
    )
    .unwrap();
    std::fs::write(
        first.path().join("shared.toml"),
        "[search]\nmode='lexical'\n[project]\nexclude=['fixture.rs']\n",
    )
    .unwrap();
    let mut client =
        NativeClient::start_with_args(first.path(), &["--config", "shared.toml"]).await;
    let status = client
        .call("index_status", json!({"root": second.path()}))
        .await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    assert_eq!(
        status["result"]["structuredContent"]["index_dir"],
        json!(std::fs::canonicalize(second.path())
            .unwrap()
            .join(".solascope"))
    );
    let result = client
        .call(
            "search_code",
            json!({"root": second.path(), "query": "mcp_fixture_symbol"}),
        )
        .await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(result["result"]["structuredContent"]["mode"], "lexical");
    assert!(result["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    client.close().await;
}

#[tokio::test]
async fn invalid_roots_and_unindexed_projects_do_not_stop_the_session_or_build_indexes() {
    let indexed = fixture(true).await;
    let unindexed = fixture(false).await;
    let mut client = NativeClient::start(indexed.path()).await;
    for root in [
        json!(""),
        json!("  "),
        json!("bad\u{0000}root"),
        json!("x".repeat(4097)),
        json!("missing-directory"),
        json!("fixture.rs"),
    ] {
        let result = client.call("index_status", json!({"root": root})).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["error"]["exit_code"], 2,
            "{result}"
        );
    }
    for (tool, arguments) in [
        ("index_status", json!({"root": unindexed.path()})),
        (
            "search_code",
            json!({"root": unindexed.path(), "query": "mcp_fixture_symbol", "mode": "lexical"}),
        ),
        (
            "find_symbol",
            json!({"root": unindexed.path(), "name": "mcp_fixture_symbol"}),
        ),
    ] {
        let result = client.call(tool, arguments).await;
        assert_eq!(
            result["result"]["structuredContent"]["error"]["exit_code"], 3,
            "{result}"
        );
    }
    let context = client
        .call(
            "get_code_context",
            json!({"root": unindexed.path(), "path": "fixture.rs", "line": 1}),
        )
        .await;
    assert_eq!(context["result"]["isError"], false, "{context}");
    for path in ["../outside.rs", "/etc/passwd", "C:\\outside.rs", ".env"] {
        let result = client
            .call(
                "get_code_context",
                json!({"root": unindexed.path(), "path": path, "line": 1}),
            )
            .await;
        assert_eq!(result["result"]["isError"], true, "{result}");
    }
    assert!(!unindexed.path().join(".solascope").exists());
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    client.close().await;
}

#[tokio::test]
async fn startup_defaults_and_request_overrides_apply_to_both_search_tools() {
    let root = fixture(false).await;
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("src/filtered.rs"),
        "pub fn mcp_fixture_symbol() {}\npub fn mcp_fixture_symbol_extra() {}\n",
    )
    .unwrap();
    let indexed = command(root.path())
        .args(["index", "--json"])
        .output()
        .await
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let mut client = NativeClient::start_with_args(
        root.path(),
        &[
            "--mode",
            "lexical",
            "--limit",
            "1",
            "--path",
            "src/",
            "--no-rerank",
        ],
    )
    .await;
    for (tool, key) in [("search_code", "query"), ("find_symbol", "name")] {
        let inherited = client.call(tool, json!({key: "mcp_fixture_symbol"})).await;
        assert_eq!(inherited["result"]["isError"], false, "{inherited}");
        let results = inherited["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 1, "{inherited}");
        assert_eq!(results[0]["relative_path"], "src/filtered.rs");

        let overridden = client
            .call(
                tool,
                json!({
                    key: "mcp_fixture_symbol", "limit": 10, "path_filter": "**/fixture.rs"
                }),
            )
            .await;
        assert_eq!(overridden["result"]["isError"], false, "{overridden}");
        let results = overridden["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 1, "{overridden}");
        assert_eq!(results[0]["relative_path"], "fixture.rs");

        let cleared = client
            .call(
                tool,
                json!({
                    key: "mcp_fixture_symbol", "limit": 10, "path_filter": ""
                }),
            )
            .await;
        assert_eq!(cleared["result"]["isError"], false, "{cleared}");
        let results = cleared["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap();
        assert!(
            results
                .iter()
                .any(|result| result["relative_path"] == "fixture.rs"),
            "{cleared}"
        );
        assert!(
            results
                .iter()
                .any(|result| result["relative_path"] == "src/filtered.rs"),
            "{cleared}"
        );
    }
    let hybrid = client
        .call(
            "search_code",
            json!({
                "query": "mcp_fixture_symbol", "mode": "hybrid", "rerank": false
            }),
        )
        .await;
    assert_eq!(hybrid["result"]["isError"], false, "{hybrid}");
    assert_eq!(hybrid["result"]["structuredContent"]["mode"], "hybrid");
    assert!(!hybrid["result"]["structuredContent"]["warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    client.close().await;
}

#[tokio::test]
async fn lexical_reranking_uses_dotenv_and_can_be_disabled_per_request() {
    let root = fixture(true).await;
    let settings = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let config = settings.path().join("mcp.toml");
    let api_key = "fixture-key";
    std::fs::write(&config, format!(
        "[privacy]\nremote_opt_in=true\n[rerank]\nprovider='siliconflow'\nmodel='test-reranker'\nendpoint='{}'\napi_key_env='SOLASCOPE_MCP_RERANK_KEY'\nfail_open=false\n",
        server.uri(),
    )).unwrap();
    std::fs::write(
        settings.path().join(".env"),
        format!("SOLASCOPE_MCP_RERANK_KEY={api_key}\n"),
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {api_key}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"index": 0, "relevance_score": 0.9}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = NativeClient::start_with_args(
        root.path(),
        &["--config", config.to_str().unwrap(), "--mode", "lexical"],
    )
    .await;
    assert!(server.received_requests().await.unwrap().is_empty());
    let ranked = client
        .call("search_code", json!({"query": "mcp_fixture_symbol"}))
        .await;
    assert_eq!(ranked["result"]["isError"], false, "{ranked}");
    assert!(
        ranked["result"]["structuredContent"]["results"][0]["match_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("rerank"))
    );
    let unranked = client
        .call(
            "search_code",
            json!({"query": "mcp_fixture_symbol", "rerank": false}),
        )
        .await;
    assert_eq!(unranked["result"]["isError"], false, "{unranked}");
    assert!(
        !unranked["result"]["structuredContent"]["results"][0]["match_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("rerank"))
    );
    for (tool, arguments) in [
        ("find_symbol", json!({"name": "mcp_fixture_symbol"})),
        ("get_code_context", json!({"path": "fixture.rs", "line": 1})),
        ("index_status", json!({})),
    ] {
        let result = client.call(tool, arguments).await;
        assert_eq!(result["result"]["isError"], false, "{result}");
        assert!(!result.to_string().contains(api_key));
    }
    client.close().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn running_server_reads_new_generations_and_current_visibility() {
    let root = fixture(true).await;
    let mut client = NativeClient::start(root.path()).await;
    let status = client.call("index_status", json!({})).await;
    let original_generation = status["result"]["structuredContent"]["index_version"]
        .as_u64()
        .unwrap();
    let corrected = client
        .call("find_symbol", json!({"name": "mcp_fixture_symbl"}))
        .await;
    assert_eq!(corrected["result"]["isError"], false, "{corrected}");
    assert!(
        corrected["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["match_sources"]
                .as_array()
                .unwrap()
                .contains(&json!("fuzzy"))),
        "{corrected}"
    );

    std::fs::write(
        root.path().join("fixture.rs"),
        "pub fn mcp_replacement_symbol() {}\n",
    )
    .unwrap();
    let stale = client
        .call("find_symbol", json!({"name": "mcp_fixture_symbol"}))
        .await;
    assert_eq!(stale["result"]["isError"], false, "{stale}");
    assert_eq!(
        stale["result"]["structuredContent"]["results"][0]["stale"], true,
        "{stale}"
    );
    let context = client
        .call("get_code_context", json!({"path": "fixture.rs", "line": 1}))
        .await;
    assert_eq!(context["result"]["isError"], false, "{context}");
    assert!(context["result"]["structuredContent"]["lines"][0]["text"]
        .as_str()
        .unwrap()
        .contains("mcp_replacement_symbol"));

    let indexed = command(root.path())
        .args(["index", "--json"])
        .output()
        .await
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");
    let refreshed = client
        .call("find_symbol", json!({"name": "mcp_replacement_symbol"}))
        .await;
    assert_eq!(refreshed["result"]["isError"], false, "{refreshed}");
    assert!(
        refreshed["result"]["structuredContent"]["index_version"]
            .as_u64()
            .unwrap()
            > original_generation
    );
    assert_eq!(
        refreshed["result"]["structuredContent"]["results"][0]["stale"],
        false
    );

    std::fs::write(root.path().join(".solascopeignore"), "fixture.rs\n").unwrap();
    let hidden = client
        .call("find_symbol", json!({"name": "mcp_replacement_symbol"}))
        .await;
    assert_eq!(hidden["result"]["isError"], false, "{hidden}");
    assert!(hidden["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    let denied = client
        .call("get_code_context", json!({"path": "fixture.rs", "line": 1}))
        .await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    client.close().await;
}

#[tokio::test]
async fn outdated_chunking_errors_preserve_index_and_allow_source_context() {
    let root = fixture(true).await;
    let database = root.path().join(".solascope/index.sqlite3");
    let original = std::fs::read(&database).unwrap();
    let settings = tempfile::tempdir().unwrap();
    let config = settings.path().join("parser.toml");
    std::fs::write(&config, "[parser]\nmax_chunk_tokens=256\n").unwrap();
    let mut client =
        NativeClient::start_with_args(root.path(), &["--config", config.to_str().unwrap()]).await;
    for (tool, arguments) in [
        (
            "search_code",
            json!({"query": "mcp_fixture_symbol", "mode": "lexical"}),
        ),
        ("find_symbol", json!({"name": "mcp_fixture_symbol"})),
        ("index_status", json!({})),
    ] {
        let result = client.call(tool, arguments).await;
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            result["result"]["structuredContent"]["error"]["exit_code"], 3,
            "{result}"
        );
    }
    let context = client
        .call("get_code_context", json!({"path": "fixture.rs", "line": 1}))
        .await;
    assert_eq!(context["result"]["isError"], false, "{context}");
    client.close().await;
    assert_eq!(std::fs::read(&database).unwrap(), original);
}

#[tokio::test]
async fn database_error_is_a_tool_result_and_does_not_stop_server() {
    let root = fixture(false).await;
    let mut client = NativeClient::start(root.path()).await;
    let error = client
        .call(
            "search_code",
            json!({ "query": "fixture", "mode": "lexical" }),
        )
        .await;
    assert_eq!(error["result"]["isError"], true, "{error}");
    assert_eq!(
        error["result"]["structuredContent"]["error"]["exit_code"],
        solascope_core::Error::IndexMissing.exit_code()
    );
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], true, "{status}");
    assert_eq!(
        status["result"]["structuredContent"]["error"]["exit_code"],
        3
    );
    let context = client
        .call("get_code_context", json!({"path": "fixture.rs", "line": 1}))
        .await;
    assert_eq!(context["result"]["isError"], false, "{context}");
    assert!(context["result"]["structuredContent"]["lines"][0]["text"]
        .as_str()
        .unwrap()
        .contains("mcp_fixture_symbol"));
    assert!(!root.path().join(".solascope/index.sqlite3").exists());
    let list = client.request("tools/list", json!({})).await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 6);
    client.close().await;
}

#[tokio::test]
async fn cancellation_notification_leaves_session_usable() {
    let root = fixture(true).await;
    let mut client = NativeClient::start(root.path()).await;
    // Unknown/already-completed cancellations are legal and must not end a session.
    client.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 999, "reason": "test cancellation" } })).await;
    let status = client.call("index_status", json!({})).await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    client.close().await;
}

#[tokio::test]
async fn eof_before_initialization_exits_cleanly_without_stdout() {
    let root = fixture(false).await;
    let output = timeout(
        WAIT,
        command(root.path())
            .arg("mcp")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn rmcp_client_interoperates_with_stdio_server_and_closes() {
    let root = fixture(true).await;
    let mut process = command(root.path());
    process.args(["mcp", "--root", root.path().to_str().unwrap()]);
    let transport = TokioChildProcess::new(process).unwrap();
    let client = timeout(WAIT, ().serve(transport)).await.unwrap().unwrap();
    let tools = timeout(WAIT, client.list_tools(None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tools.tools.len(), 6);
    for tool in ["index", "reindex"] {
        let request = CallToolRequestParams::new(tool).with_arguments(Default::default());
        let result = timeout(WAIT, client.call_tool(request))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["rebuilt"],
            tool == "reindex"
        );
    }
    let request = CallToolRequestParams::new("search_code").with_arguments(
        json!({
            "query": "mcp_fixture_symbol", "mode": "lexical", "rerank": false
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let result = timeout(WAIT, client.call_tool(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["results"][0]["relative_path"],
        "fixture.rs"
    );
    let request = CallToolRequestParams::new("get_code_context").with_arguments(
        json!({ "path": "../escape", "line": 1 })
            .as_object()
            .unwrap()
            .clone(),
    );
    let error = timeout(WAIT, client.call_tool(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(error.is_error, Some(true));
    let request = CallToolRequestParams::new("index_status").with_arguments(Default::default());
    assert_eq!(
        timeout(WAIT, client.call_tool(request))
            .await
            .unwrap()
            .unwrap()
            .is_error,
        Some(false)
    );
    timeout(WAIT, client.cancel()).await.unwrap().unwrap();
}
