use std::{path::Path, process::Stdio, time::Duration};

use rmcp::{model::CallToolRequestParams, transport::TokioChildProcess, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

const BINARY: &str = env!("CARGO_BIN_EXE_solascope");
const WAIT: Duration = Duration::from_secs(10);

fn command(root: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("user-config"));
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
        let mut child = command(root)
            .args(["mcp", "--root", root.to_str().unwrap()])
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
