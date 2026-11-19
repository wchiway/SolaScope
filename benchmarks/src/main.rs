//! `solascope-bench`: retrieval-quality and performance measurements for
//! SolaScope. The report goes to stdout; progress, notices and errors go to
//! stderr.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use solascope_bench::perf::{self, PerfOptions};
use solascope_bench::quality::{self, QualityOptions};
use solascope_bench::queries;
use solascope_config::Config;
use solascope_core::{Error, Result, MAX_RESULTS};

/// Far more repetitions than any latency distribution needs; keeps a typo from
/// turning into an hours-long run.
const MAX_RUNS: usize = 100_000;

#[derive(Debug, Parser)]
#[command(
    name = "solascope-bench",
    version,
    about = "Reproducible retrieval-quality and performance measurements for SolaScope",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Score an annotated query set once per retrieval variant the configuration enables.
    Quality(QualityArgs),
    /// Measure indexing time, index size, peak memory and in-process search latency
    /// against a temporary index directory.
    Perf(PerfArgs),
}

#[derive(Debug, Args)]
struct QualityArgs {
    /// Project root to evaluate.
    #[arg(long, value_name = "DIR")]
    root: PathBuf,
    /// Annotated queries: a JSON array of {category, query, path, symbol, start_line?, end_line?}.
    #[arg(long, value_name = "FILE")]
    queries: PathBuf,
    /// Configuration file to use instead of <root>/.solascope.toml.
    #[arg(long, value_name = "TOML")]
    config: Option<PathBuf>,
    /// Results requested per query (1..=100).
    #[arg(long, default_value_t = 10, value_parser = parse_limit)]
    limit: usize,
    /// Evaluate the existing index as it is instead of updating it first.
    #[arg(long)]
    no_index: bool,
    /// Print one JSON document instead of tables.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PerfArgs {
    /// Project root to measure. Its sources are only read and its own index is not used.
    #[arg(long, value_name = "DIR")]
    root: PathBuf,
    /// Queries for the latency section (the `query` strings of a JSON array of objects).
    /// Without it that section is skipped.
    #[arg(long, value_name = "FILE")]
    queries: Option<PathBuf>,
    /// Configuration file to use instead of <root>/.solascope.toml.
    #[arg(long, value_name = "TOML")]
    config: Option<PathBuf>,
    /// Measured repetitions per query and variant, after one separate first run.
    #[arg(long, default_value_t = 30, value_parser = parse_runs)]
    runs: usize,
    /// Keep the temporary index directory instead of removing it.
    #[arg(long)]
    keep_index: bool,
    /// Print one JSON document instead of tables.
    #[arg(long)]
    json: bool,
}

impl Command {
    fn json(&self) -> bool {
        match self {
            Self::Quality(args) => args.json,
            Self::Perf(args) => args.json,
        }
    }
}

fn parse_limit(value: &str) -> std::result::Result<usize, String> {
    match value.parse::<usize>() {
        Ok(limit) if (1..=MAX_RESULTS).contains(&limit) => Ok(limit),
        _ => Err(format!(
            "limit must be an integer between 1 and {MAX_RESULTS}"
        )),
    }
}

fn parse_runs(value: &str) -> std::result::Result<usize, String> {
    match value.parse::<usize>() {
        Ok(runs) if (1..=MAX_RUNS).contains(&runs) => Ok(runs),
        _ => Err(format!("runs must be an integer between 1 and {MAX_RUNS}")),
    }
}

fn load_config(root: &Path, config: Option<&Path>) -> Result<Config> {
    if !root.is_dir() {
        return Err(Error::InvalidInput(format!(
            "--root {} is not a directory",
            root.display()
        )));
    }
    Config::load(Some(root), config)
}

fn document<T: Serialize>(
    report: &T,
    json: bool,
    render: impl FnOnce(&T) -> String,
) -> Result<String> {
    if !json {
        return Ok(render(report));
    }
    let mut text = serde_json::to_string(report)
        .map_err(|error| Error::Storage(format!("serialize report: {error}")))?;
    text.push('\n');
    Ok(text)
}

/// Runs one subcommand and returns the finished document, so that nothing is
/// written to stdout while a measurement is in progress.
async fn execute(command: &Command) -> Result<String> {
    let mut progress = |line: &str| eprintln!("{line}");
    match command {
        Command::Quality(args) => {
            let queries = queries::load_annotated(&args.queries)?;
            let config = load_config(&args.root, args.config.as_deref())?;
            let options = QualityOptions {
                limit: args.limit,
                index: !args.no_index,
                queries_file: Some(args.queries.clone()),
            };
            let report = quality::run_quality(config, &queries, &options, &mut progress).await?;
            document(&report, args.json, quality::render_quality)
        }
        Command::Perf(args) => {
            let queries = args
                .queries
                .as_deref()
                .map(queries::load_query_strings)
                .transpose()?;
            let mut config = load_config(&args.root, args.config.as_deref())?;
            let annotation_notice = args
                .queries
                .as_deref()
                .and_then(|file| queries::exclude_query_file(&mut config, file));
            if let Some(notice) = &annotation_notice {
                progress(&format!("notice: {notice}"));
            }
            let options = PerfOptions {
                runs: args.runs,
                keep_index: args.keep_index,
            };
            let mut report =
                perf::run_perf(config, queries.as_ref(), &options, &mut progress).await?;
            if let Some(notice) = annotation_notice {
                report.notices.push(notice);
            }
            document(&report, args.json, perf::render_perf)
        }
    }
}

fn print(document: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(document.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn report_error(error: &Error, json: bool) -> i32 {
    let code = error.exit_code();
    if json {
        eprintln!(
            "{}",
            serde_json::json!({ "error": { "message": error.to_string(), "exit_code": code } })
        );
    } else {
        eprintln!("error: {error}");
        if matches!(error, Error::WriterBusy) {
            eprintln!("hint: `quality --no-index` evaluates the existing index without taking the writer lock");
        }
    }
    code
}

fn main() {
    let cli = Cli::parse();
    let json = cli.command.json();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => std::process::exit(report_error(&Error::Io(error), json)),
    };
    // Dropping the run on Ctrl-C releases what it holds, which removes the
    // temporary index directory of `perf`.
    let outcome = runtime.block_on(async {
        tokio::select! {
            result = execute(&cli.command) => result,
            Ok(()) = tokio::signal::ctrl_c() => Err(Error::Cancelled),
        }
    });
    runtime.shutdown_background();
    let code = match outcome.and_then(|document| print(&document)) {
        Ok(()) => 0,
        Err(error) => report_error(&error, json),
    };
    std::process::exit(code);
}
