use std::{
    collections::VecDeque,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    service::{RequestContext, ServerInitializeError},
    tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solascope_config::Config;
use solascope_core::{Error, Result, SearchMode, SearchRequest, SymbolRequest};
use solascope_engine::Engine;
use tokio_util::sync::CancellationToken;

use crate::cli::{context_request, parse_query, Mode};

#[derive(Clone)]
pub(crate) struct Defaults {
    pub limit: Option<usize>,
    pub mode: Option<SearchMode>,
    pub path_filter: Option<String>,
    pub rerank: bool,
}

#[derive(Clone)]
struct CodeServer {
    engine: Engine,
    defaults: Defaults,
    cwd: PathBuf,
    config_path: Option<PathBuf>,
    projects: Arc<Mutex<VecDeque<(PathBuf, Engine)>>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    /// Project directory; omit to use the startup default (normally the current
    /// directory). Relative roots resolve from the server's working directory.
    /// Uses this project's .solascope index unless its configuration overrides it.
    #[schemars(length(min = 1, max = 4096))]
    root: Option<String>,
    /// Question, symbol, keyword, or error message; 1–4096 UTF-8 bytes, not blank.
    #[schemars(length(min = 1, max = 4096))]
    query: String,
    /// Number of results, from 1 to 100; defaults to server configuration.
    #[schemars(range(min = 1, max = 100))]
    limit: Option<usize>,
    /// lexical: FTS5 only; semantic: embeddings required; hybrid: lexical, symbols,
    /// and optional embeddings. Defaults to server configuration.
    mode: Option<Mode>,
    /// Project-relative path prefix or glob (for example, src/ or **/*.rs).
    /// Omit to inherit the server filter; an empty string clears it.
    path_filter: Option<String>,
    /// Use the configured reranker in any mode. Defaults to server configuration;
    /// false skips reranking. Lexical mode skips embeddings, but may still rerank.
    rerank: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SymbolParams {
    /// Project directory; omit to use the startup default (normally the current
    /// directory). Relative roots resolve from the server's working directory.
    /// Uses this project's .solascope index unless its configuration overrides it.
    #[schemars(length(min = 1, max = 4096))]
    root: Option<String>,
    /// Symbol name, qualified name, or fragment; 1–4096 UTF-8 bytes, not blank.
    /// Matching prefers exact names, then prefixes, substrings, and spelling correction.
    #[schemars(length(min = 1, max = 4096))]
    name: String,
    /// Definition kind, for example function, method, class, struct, enum, or trait.
    kind: Option<String>,
    /// Project-relative path prefix or glob. Omit to inherit the server filter;
    /// an empty string clears it.
    path_filter: Option<String>,
    /// Number of results, from 1 to 100; defaults to server configuration.
    #[schemars(range(min = 1, max = 100))]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContextParams {
    /// Project directory; omit to use the startup default (normally the current
    /// directory). Relative roots resolve from the server's working directory.
    /// Uses this project's .solascope index unless its configuration overrides it.
    #[schemars(length(min = 1, max = 4096))]
    root: Option<String>,
    /// Current source file relative to the selected root, subject to ignore and
    /// sensitive-path rules. Does not require an index; use paths from search results.
    #[schemars(length(min = 1))]
    path: String,
    /// Inclusive range start. Requires end_line and excludes line/before/after.
    #[schemars(range(min = 1))]
    start_line: Option<usize>,
    /// Inclusive range end; must be >= start_line, with at most 200 lines total.
    #[schemars(range(min = 1))]
    end_line: Option<usize>,
    /// Center line; defaults to 1 when no range is supplied.
    #[schemars(range(min = 1))]
    line: Option<usize>,
    /// Lines before center; defaults to 10. Total is limited to 200 lines / 64 KiB.
    #[schemars(range(min = 0, max = 199))]
    before: Option<usize>,
    /// Lines after center; defaults to 10. The combined range must fit in 200 lines.
    #[schemars(range(min = 0, max = 199))]
    after: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectParams {
    /// Project directory; omitted uses the startup default (normally the current
    /// directory). Relative roots resolve from the server's working directory.
    /// Uses this project's .solascope index unless its configuration overrides it.
    #[schemars(length(min = 1, max = 4096))]
    root: Option<String>,
}

impl CodeServer {
    async fn select_engine(&self, root: Option<String>) -> Result<Engine> {
        let Some(root) = root else {
            return Ok(self.engine.clone());
        };
        let server = self.clone();
        blocking(move || server.project_engine(&root)).await
    }

    fn project_engine(&self, root: &str) -> Result<Engine> {
        parse_query(root).map_err(Error::InvalidInput)?;
        let root = self.cwd.join(root).canonicalize().map_err(|_| {
            Error::InvalidInput("root must name an existing project directory".into())
        })?;
        if !root.is_dir() {
            return Err(Error::InvalidInput("root must be a directory".into()));
        }
        if root == self.engine.config().project.root {
            return Ok(self.engine.clone());
        }
        let mut projects = self
            .projects
            .lock()
            .map_err(|_| Error::Storage("MCP project cache lock was poisoned".into()))?;
        if let Some(position) = projects.iter().position(|(path, _)| path == &root) {
            let entry = projects
                .remove(position)
                .expect("cached project position exists");
            let engine = entry.1.clone();
            projects.push_back(entry);
            return Ok(engine);
        }
        let engine = Engine::new(Config::load(Some(&root), self.config_path.as_deref())?)?;
        // Retain model adapters and query caches without growing with every visited project.
        if projects.len() == 8 {
            projects.pop_front();
        }
        projects.push_back((root, engine.clone()));
        Ok(engine)
    }
}

#[tool_router]
impl CodeServer {
    #[tool(
        description = "Search indexed code by question, symbol, keyword, or error message. Lexical uses FTS5 without embeddings; hybrid combines lexical, symbols, and optional embeddings; semantic requires embeddings. Returns ranked paths, symbols, line ranges, snippets, match_sources, stale flags, warnings, and timings from one index generation. For spelling correction use find_symbol; for current source use get_code_context. Requires an existing index; run solascope index separately.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<solascope_core::SearchResponse>(),
        annotations(read_only_hint = true, destructive_hint = false, open_world_hint = true)
    )]
    async fn search_code(
        &self,
        Parameters(params): Parameters<SearchParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let result = async {
            parse_query(&params.query).map_err(Error::InvalidInput)?;
            let engine = self.select_engine(params.root).await?;
            let request = SearchRequest {
                query: params.query,
                mode: params
                    .mode
                    .map(Into::into)
                    .or(self.defaults.mode)
                    .unwrap_or(engine.config().search.mode),
                limit: params
                    .limit
                    .or(self.defaults.limit)
                    .unwrap_or(engine.config().search.limit),
                path_filter: params
                    .path_filter
                    .or_else(|| self.defaults.path_filter.clone())
                    .filter(|path| !path.is_empty()),
                rerank: params.rerank.unwrap_or(self.defaults.rerank),
            };
            request.validate()?;
            engine.search(request).await
        };
        tool_result(interruptible(context.ct, result).await)
    }

    #[tool(
        description = "Find indexed symbol definitions by exact name, prefix, substring, or limited spelling correction, optionally filtered by definition kind and relative path/glob. Corrected results include fuzzy in match_sources. Returns the same ranked-location structure as search_code, with stale flags and warnings. Does not call embedding or reranking models. Requires an existing index.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<solascope_core::SearchResponse>(),
        annotations(read_only_hint = true, destructive_hint = false, open_world_hint = false)
    )]
    async fn find_symbol(
        &self,
        Parameters(params): Parameters<SymbolParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = match interruptible(
            context.ct.clone(),
            self.select_engine(params.root.clone()),
        )
        .await
        {
            Ok(engine) => engine,
            Err(error) => return tool_error(error),
        };
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
                        path_filter: params
                            .path_filter
                            .or(defaults.path_filter)
                            .filter(|path| !path.is_empty()),
                        limit: params
                            .limit
                            .or(defaults.limit)
                            .unwrap_or(engine.config().search.limit),
                    };
                    request.validate()?;
                    engine.find_symbol(request)
                }),
            )
            .await,
        )
    }

    #[tool(
        description = "Read current numbered source inside the selected project root, including when an index is missing or stale. Supply an inclusive start_line/end_line pair or a center line with optional before/after. Returns relative_path, start_line, end_line, and numbered lines. Maximum 200 lines and 64 KiB; ignore and sensitive-path rules apply. Does not call models.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<solascope_core::ContextResponse>(),
        annotations(read_only_hint = true, destructive_hint = false, open_world_hint = false)
    )]
    async fn get_code_context(
        &self,
        Parameters(params): Parameters<ContextParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = match interruptible(
            context.ct.clone(),
            self.select_engine(params.root.clone()),
        )
        .await
        {
            Ok(engine) => engine,
            Err(error) => return tool_error(error),
        };
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
        description = "Inspect the selected project's index generation, file/chunk/vector/cache counts, relocated_vectors, embedding fingerprint, last update, and unfinished jobs. Relocated vectors are approximate reuse after file moves; solascope index --rebuild refreshes them. Missing indexes and outdated schema or chunking rules return an error. Does not probe models, check embedding compatibility, create, migrate, or update an index.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<solascope_core::IndexStatus>(),
        annotations(read_only_hint = true, destructive_hint = false, open_world_hint = false)
    )]
    async fn index_status(
        &self,
        Parameters(params): Parameters<ProjectParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let engine = match interruptible(
            context.ct.clone(),
            self.select_engine(params.root.clone()),
        )
        .await
        {
            Ok(engine) => engine,
            Err(error) => return tool_error(error),
        };
        tool_result(interruptible(context.ct, blocking(move || engine.status())).await)
    }
}

#[tool_handler]
impl ServerHandler for CodeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("solascope", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read-only code search. No startup path is required: omit root to use the startup directory (normally the current working directory), or pass root on any tool call to select a project. Relative roots resolve from the server's working directory. Each project's default index is its .solascope directory; selecting a project does not change subsequent calls' default. Use index_status to inspect the resolved root and index directory. Use search_code for questions, keywords, or error messages; find_symbol for definitions and spelling correction; get_code_context for current source, even without an index; and index_status for generation and counts. Search and symbol tools read the latest committed index generation on each call. Check warnings and stale flags before relying on indexed locations. Optional models follow server configuration and privacy settings. Run solascope index or watch separately to build or refresh the index, and doctor for diagnostics; tools never create, migrate, or update it. Tool errors do not end the session.")
    }
}

pub(crate) async fn serve(
    engine: Engine,
    defaults: Defaults,
    config_path: Option<PathBuf>,
    cancel: CancellationToken,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config_path = config_path.map(|path| cwd.join(path));
    let server = CodeServer {
        engine,
        defaults,
        cwd,
        config_path,
        projects: Arc::new(Mutex::new(VecDeque::new())),
    };
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
