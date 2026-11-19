use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::Deserialize;
use solascope_core::{ContextRequest, Error, Result, SearchMode, MAX_CONTEXT_LINES};

#[derive(Debug, Parser)]
#[command(
    name = "solascope",
    bin_name = "solascope",
    version,
    about = "Local-first code indexing and search",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Explicit configuration file.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Project root (defaults to the current directory; positional paths take precedence).
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    /// Emit one JSON result; diagnostics always go to stderr.
    #[arg(long, global = true)]
    pub json: bool,
    /// Maximum search results (1..=100); otherwise use configuration.
    #[arg(long, global = true, value_parser = parse_limit)]
    pub limit: Option<usize>,
    /// Retrieval mode; otherwise use configuration.
    #[arg(long, global = true, value_enum)]
    pub mode: Option<Mode>,
    /// Disable result reranking.
    #[arg(long, global = true)]
    pub no_rerank: bool,
    /// Restrict search to a project-relative path or prefix.
    #[arg(long = "path", global = true)]
    pub path_filter: Option<String>,
    /// Enable diagnostic logs on stderr.
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create the user configuration under ~/.solascope without overwriting it.
    Init {
        /// Probe models from the supplied configuration using a fixed sentence.
        #[arg(long)]
        probe: bool,
    },
    /// Recursively index the current directory or an explicit path.
    Index {
        /// Directory to index; defaults to the current directory.
        path: Option<PathBuf>,
        #[arg(long)]
        rebuild: bool,
        /// Keep watching after indexing (also enabled by index.watch).
        #[arg(long, conflicts_with = "no_watch")]
        watch: bool,
        /// Override index.watch and exit after the initial scan.
        #[arg(long, conflicts_with = "watch")]
        no_watch: bool,
        #[arg(long)]
        probe: bool,
    },
    /// Search indexed source code.
    Search {
        #[arg(value_parser = parse_query)]
        query: String,
    },
    /// Find symbol definitions.
    Symbol {
        #[arg(value_parser = parse_query)]
        name: String,
        #[arg(long, value_parser = parse_query)]
        kind: Option<String>,
    },
    /// Show index generation and model status.
    Status,
    /// Serve four read-only MCP tools over stdio.
    Mcp {
        #[arg(long)]
        probe: bool,
    },
    /// Recursively watch source changes until interrupted.
    Watch {
        /// Directory to watch; defaults to the current directory.
        path: Option<PathBuf>,
        #[arg(long)]
        probe: bool,
    },
    /// Read at most 200 lines / 64 KiB of project source.
    Context(ContextArgs),
    /// Diagnose configuration, storage, and optionally model services.
    Doctor {
        #[arg(long)]
        models: bool,
        #[arg(long)]
        prune_cache: bool,
    },
}

impl Command {
    pub fn project_path(&self) -> Option<&Path> {
        match self {
            Self::Index { path, .. } | Self::Watch { path, .. } => path.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Lexical,
    Semantic,
    Hybrid,
}

impl From<Mode> for SearchMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Lexical => Self::Lexical,
            Mode::Semantic => Self::Semantic,
            Mode::Hybrid => Self::Hybrid,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ContextArgs {
    /// Project-relative path, optionally followed by :LINE.
    pub location: String,
    #[arg(long, visible_alias = "start", value_parser = parse_line, requires = "end_line", conflicts_with = "line")]
    pub start_line: Option<usize>,
    #[arg(long, visible_alias = "end", value_parser = parse_line, requires = "start_line", conflicts_with = "line")]
    pub end_line: Option<usize>,
    #[arg(long, value_parser = parse_line)]
    pub line: Option<usize>,
    #[arg(long, value_parser = parse_radius, conflicts_with = "start_line")]
    pub before: Option<usize>,
    #[arg(long, value_parser = parse_radius, conflicts_with = "start_line")]
    pub after: Option<usize>,
}

impl ContextArgs {
    pub fn request(&self) -> Result<ContextRequest> {
        let (path, suffix) = match self.location.rsplit_once(':') {
            Some((path, line)) => {
                let line = parse_line(line).map_err(Error::InvalidInput)?;
                (path.to_owned(), Some(line))
            }
            None => (self.location.clone(), None),
        };
        if suffix.is_some()
            && (self.line.is_some() || self.start_line.is_some() || self.end_line.is_some())
        {
            return Err(Error::InvalidInput(
                "use either path:line, --line, or --start-line/--end-line".into(),
            ));
        }
        context_request(
            path,
            self.start_line,
            self.end_line,
            suffix.or(self.line),
            self.before,
            self.after,
        )
    }
}

pub(crate) fn context_request(
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    line: Option<usize>,
    before: Option<usize>,
    after: Option<usize>,
) -> Result<ContextRequest> {
    let invalid = || {
        Error::InvalidInput("context requires positive lines, a complete ascending range or a center line, and at most 200 lines".into())
    };
    if path.trim().is_empty() || path.contains('\0') {
        return Err(Error::InvalidInput("context path must not be empty".into()));
    }
    let before_value = before.unwrap_or(10);
    let after_value = after.unwrap_or(10);
    match (start_line, end_line) {
        (Some(start), Some(end)) => {
            if start == 0
                || end < start
                || end - start >= MAX_CONTEXT_LINES
                || line.is_some()
                || before.is_some()
                || after.is_some()
            {
                return Err(invalid());
            }
        }
        (None, None) => {
            let center = line.unwrap_or(1);
            if center == 0 || before_value >= MAX_CONTEXT_LINES || after_value >= MAX_CONTEXT_LINES
            {
                return Err(invalid());
            }
            let start = center.saturating_sub(before_value).max(1);
            let end = center.checked_add(after_value).ok_or_else(invalid)?;
            if end - start >= MAX_CONTEXT_LINES {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(ContextRequest {
        path,
        start_line,
        end_line,
        line: if start_line.is_none() {
            Some(line.unwrap_or(1))
        } else {
            None
        },
        before: before_value,
        after: after_value,
    })
}

pub(crate) fn parse_query(value: &str) -> std::result::Result<String, String> {
    solascope_core::validate_query(value, 1).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn parse_limit(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer")?;
    if !(1..=100).contains(&value) {
        return Err("limit must be between 1 and 100".into());
    }
    Ok(value)
}

fn parse_line(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "line must be a positive integer")?;
    if value == 0 {
        return Err("line must be a positive integer".into());
    }
    Ok(value)
}

fn parse_radius(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "context radius must be an integer")?;
    if value >= MAX_CONTEXT_LINES {
        return Err("context radius must be less than 200".into());
    }
    Ok(value)
}
