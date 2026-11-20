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
            .args(["mcp", "--root", root.to_str().unwrap()])
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
            "index_status",
            "search_code"
        ]
    );
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert_eq!(tool["annotations"]["destructiveHint"], false, "{tool}");
        assert_eq!(
            tool["annotations"]["openWorldHint"],
            tool["name"] == "search_code",
            "{tool}"
        );
        assert_eq!(tool["outputSchema"]["type"], "object", "{tool}");
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
    assert!(schema["properties"].get("root").is_none());
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
        json!({ "query": "x", "root": "/tmp" }),
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
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 4);
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
    assert_eq!(tools.tools.len(), 4);
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
