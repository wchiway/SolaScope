use std::future::Future;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    service::{RequestContext, ServerInitializeError},
    tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solascope_core::{Error, Result, SearchMode, SearchRequest, SymbolRequest};
use solascope_engine::Engine;
use tokio_util::sync::CancellationToken;

use crate::cli::{context_request, parse_query, Mode};

#[derive(Clone)]
pub(crate) struct Defaults {
    pub limit: usize,
    pub mode: SearchMode,
    pub path_filter: Option<String>,
    pub rerank: bool,
}

#[derive(Clone)]
struct CodeServer {
    engine: Engine,
    defaults: Defaults,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    /// Nonempty search text, at most 4096 UTF-8 bytes.
    #[schemars(length(min = 1, max = 4096))]
    query: String,
    /// Number of results, from 1 to 100; defaults to server configuration.
    #[schemars(range(min = 1, max = 100))]
    limit: Option<usize>,
    mode: Option<Mode>,
    /// Project-relative path/prefix; may not escape the server root.
    path_filter: Option<String>,
    rerank: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SymbolParams {
    #[schemars(length(min = 1, max = 4096))]
    name: String,
    kind: Option<String>,
    path_filter: Option<String>,
    #[schemars(range(min = 1, max = 100))]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContextParams {
    /// Source path relative to the fixed server root.
    path: String,
    /// Inclusive range start. Requires end_line and excludes line/before/after.
    #[schemars(range(min = 1))]
    start_line: Option<usize>,
    #[schemars(range(min = 1))]
    end_line: Option<usize>,
    /// Center line; defaults to 1 when no range is supplied.
    #[schemars(range(min = 1))]
    line: Option<usize>,
    /// Lines before center; defaults to 10. Total is limited to 200 lines / 64 KiB.
    #[schemars(range(min = 0, max = 199))]
    before: Option<usize>,
    #[schemars(range(min = 0, max = 199))]
    after: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[tool_router]
impl CodeServer {
    #[tool(
        description = "Search indexed source code in the fixed project root. Returns ranked locations, snippets, scores, and warnings; never creates an index."
    )]
    async fn search_code(
        &self,
        Parameters(params): Parameters<SearchParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let result = async {
            parse_query(&params.query).map_err(Error::InvalidInput)?;
            let request = SearchRequest {
                query: params.query,
                mode: params.mode.map(Into::into).unwrap_or(self.defaults.mode),
                limit: params.limit.unwrap_or(self.defaults.limit),
                path_filter: params
                    .path_filter
                    .or_else(|| self.defaults.path_filter.clone()),
                rerank: params.rerank.unwrap_or(self.defaults.rerank),
            };
            request.validate()?;
            self.engine.search(request).await
        };
        tool_result(interruptible(context.ct, result).await)
    }

    #[tool(
        description = "Find symbol definitions in the existing project index, optionally filtered by kind and relative path."
    )]
    async fn find_symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = self.engine.clone();
        let defaults = self.defaults.clone();
        tool_result(
            interruptible(
                context.ct,
                blocking(move || {
                    parse_query(&params.name).map_err(Error::InvalidInput)?;
                    if let Some(kind) = &params.kind {
                        parse_query(kind).map_err(Error::InvalidInput)?;
                    }
                    let request = SymbolRequest {
                        name: params.name,
                        kind: params.kind,
                        path_filter: params.path_filter.or(defaults.path_filter),
                        limit: params.limit.unwrap_or(defaults.limit),
                    };
                    request.validate()?;
                    engine.find_symbol(request)
                }),
            )
            .await,
        )
    }

    #[tool(
        description = "Read numbered source context inside the fixed root. Supply an inclusive start_line/end_line pair or a center line with optional before/after. Maximum 200 lines and 64 KiB."
    )]
    async fn get_code_context(
        &self,
        Parameters(params): Parameters<ContextParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = self.engine.clone();
        tool_result(
            interruptible(
                context.ct,
                blocking(move || {
                    let request = context_request(
                        params.path,
                        params.start_line,
                        params.end_line,
                        params.line,
                        params.before,
                        params.after,
                    )?;
                    engine.context(request)
                }),
            )
            .await,
        )
    }

    #[tool(
        description = "Inspect current index generation, counts, model identity, and last update for the fixed project. Does not index or modify source files."
    )]
    async fn index_status(
        &self,
        Parameters(_params): Parameters<EmptyParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = self.engine.clone();
        tool_result(interruptible(context.ct, blocking(move || engine.status())).await)
    }
}

#[tool_handler]
impl ServerHandler for CodeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("solascope", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read-only code search for one fixed project root. Index the project with `solascope index` before searching. Tool errors do not end the session.")
    }
}

pub(crate) async fn serve(
    engine: Engine,
    defaults: Defaults,
    cancel: CancellationToken,
) -> Result<()> {
    let server = CodeServer { engine, defaults };
    let service = match server.serve_with_ct(rmcp::transport::stdio(), cancel).await {
        Ok(service) => service,
        // Closing stdin before or during the handshake is a normal client shutdown.
        Err(ServerInitializeError::ExpectedInitializeRequest(None))
        | Err(ServerInitializeError::ConnectionClosed(_))
        | Err(ServerInitializeError::Cancelled) => return Ok(()),
        Err(error) => return Err(Error::InvalidInput(format!("MCP initialization: {error}"))),
    };
    let reason = service
        .waiting()
        .await
        .map_err(|error| Error::Storage(format!("MCP service: {error}")))?;
    if let rmcp::service::QuitReason::JoinError(error) = reason {
        return Err(Error::Storage(format!("MCP service: {error}")));
    }
    Ok(())
}

async fn blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| Error::Storage(format!("MCP tool task: {error}")))?
}

async fn interruptible<T>(
    cancel: CancellationToken,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = operation => result,
    }
}

fn tool_result<T: Serialize>(result: Result<T>) -> CallToolResult {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => tool_error(Error::Storage(format!("serialize tool result: {error}"))),
        },
        Err(error) => tool_error(error),
    }
}

fn tool_error(error: Error) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": { "message": error.to_string(), "exit_code": error.exit_code() }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_cancellation_interrupts_pending_operation() {
        let token = CancellationToken::new();
        let cancelled = token.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(interruptible(token, async move {
            started.send(()).unwrap();
            std::future::pending::<Result<()>>().await
        }));
        ready.await.unwrap();
        cancelled.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), operation)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
    }
}
