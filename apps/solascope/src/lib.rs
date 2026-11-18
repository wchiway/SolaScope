//! Shared CLI and stdio MCP entry point for both executable names.

mod cli;
mod mcp;

use std::io::{self, Write};

use clap::Parser;
use serde::Serialize;
use serde_json::json;
use solascope_config::Config;
use solascope_core::{
    ContextResponse, Error, Result, SearchRequest, SearchResponse, SymbolRequest,
};
use solascope_engine::Engine;
use tokio_util::sync::CancellationToken;

use cli::{Cli, Command};

/// Parse arguments and run either entry point, returning the shared core exit code.
/// Help and version return before configuration, database, or runtime initialization.
pub fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() {
                Error::InvalidInput(error.to_string()).exit_code()
            } else {
                0
            };
            let _ = error.print();
            return code;
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if cli.verbose { "info" } else { "warn" })
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return report_error(Error::Io(error), cli.json),
    };
    let json_output = cli.json;
    match runtime.block_on(execute(cli)) {
        Ok(()) => 0,
        Err(error) => report_error(error, json_output),
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let root = cli.command.project_path().or(cli.root.as_deref());
    // Initialization can explicitly create a new project directory.
    if matches!(cli.command, Command::Init { .. }) {
        if let Some(root) = root {
            std::fs::create_dir_all(root)?;
        }
        let config = Config::load(root, cli.config.as_deref())?;
        let path =
            solascope_config::write_default_config(&config.project.root).map_err(|error| {
                match error {
                    Error::Io(io) if io.kind() == io::ErrorKind::AlreadyExists => {
                        Error::Config(format!(
                            "{} already exists and was left unchanged",
                            config.project.root.join(".solascope.toml").display()
                        ))
                    }
                    other => other,
                }
            })?;
        if cli.json {
            return emit(&json!({ "config_path": path }), true);
        }
        writeln!(io::stdout().lock(), "Created {}", path.display())?;
        eprintln!(
            "Next: solascope index --root {}",
            config.project.root.display()
        );
        return Ok(());
    }

    let config = Config::load(root, cli.config.as_deref())?;
    let engine = Engine::new(config)?;
    let defaults = mcp::Defaults {
        limit: cli.limit.unwrap_or(engine.config().search.limit),
        mode: cli
            .mode
            .map(Into::into)
            .unwrap_or(engine.config().search.mode),
        path_filter: cli.path_filter,
        rerank: !cli.no_rerank,
    };
    match cli.command {
        Command::Init { .. } => unreachable!("init handled above"),
        Command::Index { rebuild, .. } => {
            let (cancel, signal) = interrupt_signal();
            let result = engine.index_with_cancel(rebuild, cancel).await;
            signal.abort();
            let report = result?;
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            emit(&report, cli.json)
        }
        Command::Search { query } => {
            let request = SearchRequest {
                query,
                mode: defaults.mode,
                limit: defaults.limit,
                path_filter: defaults.path_filter,
                rerank: defaults.rerank,
            };
            request.validate()?;
            let response = engine.search(request).await?;
            emit_search(&response, cli.json)
        }
        Command::Symbol { name, kind } => {
            let request = SymbolRequest {
                name,
                kind,
                path_filter: defaults.path_filter,
                limit: defaults.limit,
            };
            request.validate()?;
            emit_search(&engine.find_symbol(request)?, cli.json)
        }
        Command::Status => emit(&engine.status()?, cli.json),
        Command::Context(args) => emit_context(&engine.context(args.request()?)?, cli.json),
        Command::Doctor {
            models,
            prune_cache,
        } => {
            let pruned = if prune_cache {
                Some(engine.prune_cache()?)
            } else {
                None
            };
            let report = engine.doctor(models).await?;
            if let Some(count) = pruned {
                emit(
                    &json!({ "checks": report.checks, "healthy": report.healthy, "pruned_cache_entries": count }),
                    cli.json,
                )
            } else {
                emit(&report, cli.json)
            }
        }
        Command::Watch { .. } => {
            let (cancel, signal) = interrupt_signal();
            let result = engine.watch(cancel).await;
            signal.abort();
            match result {
                Ok(()) | Err(Error::Cancelled) => emit(&json!({ "stopped": true }), cli.json),
                Err(error) => Err(error),
            }
        }
        Command::Mcp => {
            let (cancel, signal) = interrupt_signal();
            let result = mcp::serve(engine, defaults, cancel).await;
            signal.abort();
            result
        }
    }
}

fn interrupt_signal() -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let token = CancellationToken::new();
    let signal_token = token.clone();
    let handle = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_token.cancel();
        }
    });
    (token, handle)
}

fn report_error(error: Error, json_output: bool) -> i32 {
    let code = error.exit_code();
    if json_output {
        eprintln!(
            "{}",
            json!({ "error": { "message": error.to_string(), "exit_code": code } })
        );
    } else {
        eprintln!("error: {error}");
    }
    code
}

fn emit<T: Serialize>(value: &T, json_output: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    if json_output {
        serde_json::to_writer(&mut output, value)
    } else {
        serde_json::to_writer_pretty(&mut output, value)
    }
    .map_err(|error| Error::Storage(format!("serialize output: {error}")))?;
    writeln!(output)?;
    Ok(())
}

fn emit_search(response: &SearchResponse, json_output: bool) -> Result<()> {
    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }
    if json_output {
        return emit(response, true);
    }
    let mut output = io::stdout().lock();
    for result in &response.results {
        writeln!(
            output,
            "{}:{}:{}  {}  {:.4}",
            result.relative_path,
            result.start_line,
            result.end_line,
            result.symbol.as_deref().unwrap_or(&result.kind),
            result.score
        )?;
        writeln!(output, "{}", result.snippet)?;
    }
    Ok(())
}

fn emit_context(response: &ContextResponse, json_output: bool) -> Result<()> {
    if json_output {
        return emit(response, true);
    }
    let mut output = io::stdout().lock();
    for line in &response.lines {
        writeln!(
            output,
            "{}:{}: {}",
            response.relative_path, line.line, line.text
        )?;
    }
    Ok(())
}
