//! Indexing time, index size, peak resident memory and in-process search
//! latency, measured against a throw-away index directory.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use solascope_config::Config;
use solascope_core::{Error, IndexReport, Result};
use solascope_engine::Engine;

use crate::metrics::{summarize, TimingSummary};
use crate::queries::QueryStrings;
use crate::report::{
    config_json, millis, tally, Environment, MessageCount, ModelSummary, DEBUG_BUILD_NOTICE,
};
use crate::table::{self, Align, Table};
use crate::variants::{plan, SkippedVariant, Variant};
use crate::Progress;

/// What every `peak_rss_kib` value in the report is.
pub const PEAK_RSS_METRIC: &str =
    "VmHWM from /proc/self/status: the peak resident set size of the process so far, in KiB";

/// What every search timing in the report covers.
pub const LATENCY_SCOPE: &str = "in-process wall time around one Engine::search call; excludes process start-up, argument parsing, configuration loading and output formatting";

/// Why the latency section is absent when no query file is given.
pub const LATENCY_NEEDS_QUERIES: &str =
    "no query file was given (--queries); the harness has no built-in queries";

#[derive(Debug, Clone)]
pub struct PerfOptions {
    /// Measured repetitions per query and variant, after one separate first run.
    pub runs: usize,
    /// Leave the temporary index directory in place.
    pub keep_index: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerfReport {
    pub benchmark: &'static str,
    pub environment: Environment,
    pub root: String,
    pub models: ModelSummary,
    /// The temporary index directory every measurement used.
    pub index_dir: String,
    /// Whether that directory was left in place.
    pub index_dir_kept: bool,
    pub memory: MemoryInfo,
    /// First index, immediate no-op index, rebuild; in that order.
    pub index_phases: Vec<IndexPhase>,
    /// Absent when no query file was given; see `latency_skipped`.
    pub latency: Option<LatencyReport>,
    pub latency_skipped: Option<String>,
    pub notices: Vec<String>,
    /// The effective configuration the engine ran with.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryInfo {
    pub metric: &'static str,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    /// The high-water mark before any indexing: the harness itself.
    pub start_peak_rss_kib: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// `Engine::index(false)` into the empty temporary directory.
    FirstIndex,
    /// `Engine::index(false)` again, immediately, with nothing changed.
    NoopIndex,
    /// `Engine::index(true)`.
    Rebuild,
}

impl Phase {
    pub const ALL: [Self; 3] = [Self::FirstIndex, Self::NoopIndex, Self::Rebuild];

    pub fn label(self) -> &'static str {
        match self {
            Self::FirstIndex => "first index",
            Self::NoopIndex => "no-op index",
            Self::Rebuild => "rebuild",
        }
    }

    fn rebuild(self) -> bool {
        self == Self::Rebuild
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexPhase {
    pub phase: Phase,
    /// Wall time around the single `Engine::index` call.
    pub wall_ms: f64,
    /// The engine's own account of the run: files, chunks, embedded chunks,
    /// embedding cache hits, skipped files and warnings.
    pub report: IndexReport,
    /// Sum of the lengths of all files in the index directory after the phase.
    pub index_bytes: u64,
    pub index_files: Vec<IndexFile>,
    /// Process high-water mark after the phase. It never decreases, so it is
    /// the maximum over this and all earlier phases.
    pub peak_rss_kib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexFile {
    /// Path relative to the index directory.
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyReport {
    pub scope: &'static str,
    /// Measured repetitions per query and variant; the first run is extra.
    pub runs: usize,
    /// Results requested per search (`search.limit` of the configuration).
    pub limit: usize,
    pub query_count: usize,
    pub variants: Vec<VariantLatency>,
    pub skipped_variants: Vec<SkippedVariant>,
    /// Process high-water mark after all searches.
    pub peak_rss_kib: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantLatency {
    pub variant: Variant,
    /// The first execution of each query, across queries.
    pub first_run: TimingSummary,
    /// The measured repetitions of all queries, pooled.
    pub repeated: TimingSummary,
    pub queries: Vec<QueryLatency>,
    /// Distinct search errors and how many queries hit each.
    pub errors: Vec<MessageCount>,
    /// Distinct engine warnings and how many queries produced each.
    pub warnings: Vec<MessageCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryLatency {
    pub query: String,
    /// Number of results the first run returned.
    pub result_count: Option<usize>,
    /// Wall time of the first execution; not part of `repeated`.
    pub first_ms: Option<f64>,
    /// The measured repetitions that followed the first run.
    pub repeated: TimingSummary,
    /// Set when a search failed; the remaining runs of the query are skipped.
    pub error: Option<String>,
}

/// Extracts `VmHWM` from the text of `/proc/<pid>/status`. The kernel labels
/// the unit `kB`; it is 1024 bytes.
pub fn parse_vm_hwm_kib(status: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let mut fields = line.strip_prefix("VmHWM:")?.split_whitespace();
        let value = fields.next()?.parse().ok()?;
        (fields.next() == Some("kB") && fields.next().is_none()).then_some(value)
    })
}

/// The peak resident set size of this process so far, or why it cannot be
/// read. Only Linux is supported: other systems are reported as unavailable
/// rather than approximated.
pub fn peak_rss_kib() -> std::result::Result<u64, String> {
    if !cfg!(target_os = "linux") {
        return Err(format!(
            "peak resident memory is read from /proc/self/status, which is not available on {}",
            std::env::consts::OS
        ));
    }
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("cannot read /proc/self/status: {error}"))?;
    parse_vm_hwm_kib(&status).ok_or_else(|| "/proc/self/status has no usable VmHWM line".to_owned())
}

/// Escapes glob metacharacters so that the pattern matches only `text` itself.
pub fn escape_glob(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '{' | '}') {
            escaped.extend(['[', character, ']']);
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// The exclude pattern that keeps `index_dir` out of a scan of `root`, if the
/// directory lies inside the root. With the index redirected elsewhere, the
/// scanner would otherwise read the project's real index files as source.
pub fn index_exclusion(root: &Path, index_dir: &Path) -> Option<String> {
    let parts: Vec<String> = index_dir
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str().map(escape_glob))
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn index_files(index_dir: &Path) -> io::Result<Vec<IndexFile>> {
    let mut files = Vec::new();
    let mut pending = vec![index_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // SQLite removes its sidecar files when a connection closes.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let path = entry.path();
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                files.push(IndexFile {
                    name: path
                        .strip_prefix(index_dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                    bytes: metadata.len(),
                });
            }
        }
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

async fn measure_latency(
    config: &Config,
    queries: &[String],
    runs: usize,
    progress: Progress<'_>,
) -> Result<LatencyReport> {
    let limit = config.search.limit;
    let plan = plan(config);
    for skip in &plan.skipped {
        progress(&format!("perf: skipping {}: {}", skip.variant, skip.reason));
    }
    let mut variants = Vec::with_capacity(plan.run.len());
    for variant in plan.run {
        progress(&format!(
            "perf: {variant}: {} queries x (1 first run + {runs} measured runs)",
            queries.len()
        ));
        // A fresh engine per variant: its query-vector cache starts empty, so
        // the first run of every query is cold in the same way for each variant.
        let engine = Engine::new(config.clone())?;
        let mut rows = Vec::with_capacity(queries.len());
        let mut firsts = Vec::with_capacity(queries.len());
        let mut pooled = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for query in queries {
            let request = variant.request(query, limit);
            let started = Instant::now();
            let first = engine.search(request).await;
            let first_ms = millis(started.elapsed());
            let response = match first {
                Ok(response) => response,
                Err(error) => {
                    rows.push(QueryLatency {
                        query: query.clone(),
                        result_count: None,
                        first_ms: None,
                        repeated: summarize(&[]),
                        error: Some(error.to_string()),
                    });
                    continue;
                }
            };
            let mut samples = Vec::with_capacity(runs);
            let mut failure = None;
            for _ in 0..runs {
                let request = variant.request(query, limit);
                let started = Instant::now();
                let outcome = engine.search(request).await;
                let elapsed_ms = millis(started.elapsed());
                match outcome {
                    Ok(_) => samples.push(elapsed_ms),
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                }
            }
            firsts.push(first_ms);
            pooled.extend_from_slice(&samples);
            warnings.extend(response.warnings);
            rows.push(QueryLatency {
                query: query.clone(),
                result_count: Some(response.results.len()),
                first_ms: Some(first_ms),
                repeated: summarize(&samples),
                error: failure,
            });
        }
        let errors = tally(rows.iter().filter_map(|row| row.error.as_deref()));
        for error in &errors {
            progress(&format!(
                "perf: {variant}: {} of {} queries failed: {}",
                error.queries,
                queries.len(),
                error.message
            ));
        }
        variants.push(VariantLatency {
            variant,
            first_run: summarize(&firsts),
            repeated: summarize(&pooled),
            queries: rows,
            errors,
            warnings: tally(warnings.iter().map(String::as_str)),
        });
    }
    Ok(LatencyReport {
        scope: LATENCY_SCOPE,
        runs,
        limit,
        query_count: queries.len(),
        variants,
        skipped_variants: plan.skipped,
        peak_rss_kib: peak_rss_kib().ok(),
    })
}

/// Runs the performance measurements for the project described by `config`.
///
/// `project.index_dir` is replaced by a fresh temporary directory, so an
/// existing index of the project is neither read nor written, and sources are
/// only read. The directory is removed when the run ends, also on failure,
/// unless `options.keep_index` is set. Search latency needs `queries`; without
/// them that section is skipped and the report says so.
pub async fn run_perf(
    config: Config,
    queries: Option<&QueryStrings>,
    options: &PerfOptions,
    progress: Progress<'_>,
) -> Result<PerfReport> {
    if options.runs == 0 {
        return Err(Error::InvalidInput("runs must be at least 1".into()));
    }
    let environment = Environment::detect();
    // Read before anything else so that it reflects the harness alone.
    let start_peak = peak_rss_kib();

    let scratch = tempfile::Builder::new()
        .prefix("solascope-bench-")
        .tempdir()?;
    let (index_dir, _cleanup) = if options.keep_index {
        (scratch.keep(), None)
    } else {
        (scratch.path().to_path_buf(), Some(scratch))
    };
    let index_dir = index_dir.canonicalize()?;

    let mut config = config;
    let root = config.project.root.canonicalize()?;
    let configured_index = config
        .project
        .index_dir
        .canonicalize()
        .unwrap_or_else(|_| config.project.index_dir.clone());
    if let Some(pattern) = index_exclusion(&root, &configured_index) {
        if !config.project.exclude.contains(&pattern) {
            config.project.exclude.push(pattern);
        }
    }
    config.project.index_dir = index_dir.clone();
    let engine = Engine::new(config)?;
    let config = engine.config().clone();
    let models = ModelSummary::from_config(&config);

    let mut notices = Vec::new();
    if environment.is_debug_build() {
        notices.push(DEBUG_BUILD_NOTICE.to_owned());
    }
    if models.embedding_enabled() {
        notices.push(format!(
            "embedding provider \"{}\" is enabled: the temporary index starts without a vector cache, so the first index requests an embedding for every chunk",
            models.embedding_provider
        ));
    }
    if let Some(duplicates) = queries
        .map(|queries| queries.duplicates)
        .filter(|duplicates| *duplicates > 0)
    {
        notices.push(format!(
            "{duplicates} repeated query string(s) in the query file are measured once"
        ));
    }
    let memory = match &start_peak {
        Ok(peak) => MemoryInfo {
            metric: PEAK_RSS_METRIC,
            available: true,
            unavailable_reason: None,
            start_peak_rss_kib: Some(*peak),
        },
        Err(reason) => {
            notices.push(format!("peak resident memory is unavailable: {reason}"));
            MemoryInfo {
                metric: PEAK_RSS_METRIC,
                available: false,
                unavailable_reason: Some(reason.clone()),
                start_peak_rss_kib: None,
            }
        }
    };
    for notice in &notices {
        progress(&format!("notice: {notice}"));
    }
    progress(&format!(
        "perf: temporary index directory {} ({})",
        index_dir.display(),
        if options.keep_index {
            "kept after the run"
        } else {
            "removed after the run"
        }
    ));

    let mut index_phases = Vec::with_capacity(Phase::ALL.len());
    for phase in Phase::ALL {
        progress(&format!("perf: {}", phase.label()));
        let started = Instant::now();
        let report = engine.index(phase.rebuild()).await?;
        let wall_ms = millis(started.elapsed());
        let files = index_files(&index_dir)?;
        let peak = peak_rss_kib().ok();
        for warning in &report.warnings {
            progress(&format!("warning: {warning}"));
        }
        progress(&format!(
            "perf: {}: {} ms, {} file(s) ({} updated), {} chunk(s), {} embedded",
            phase.label(),
            table::millis(Some(wall_ms)),
            report.files_total,
            report.files_updated,
            report.chunks,
            report.embedded_chunks
        ));
        if phase == Phase::NoopIndex && report.files_updated + report.files_deleted > 0 {
            let notice = format!(
                "the no-op index updated {} and deleted {} file(s): the sources changed during the run, so that row is not a no-op",
                report.files_updated, report.files_deleted
            );
            progress(&format!("notice: {notice}"));
            notices.push(notice);
        }
        index_phases.push(IndexPhase {
            phase,
            wall_ms,
            report,
            index_bytes: files.iter().map(|file| file.bytes).sum(),
            index_files: files,
            peak_rss_kib: peak,
        });
    }

    let (latency, latency_skipped) = match queries {
        Some(queries) => (
            Some(measure_latency(&config, &queries.queries, options.runs, progress).await?),
            None,
        ),
        None => {
            progress(&format!(
                "perf: skipping search latency: {LATENCY_NEEDS_QUERIES}"
            ));
            (None, Some(LATENCY_NEEDS_QUERIES.to_owned()))
        }
    };

    Ok(PerfReport {
        benchmark: "perf",
        environment,
        root: config.project.root.display().to_string(),
        models,
        index_dir: index_dir.display().to_string(),
        index_dir_kept: options.keep_index,
        memory,
        index_phases,
        latency,
        latency_skipped,
        notices,
        config: config_json(&config),
    })
}

/// Renders the report as text: indexing, memory and search latency tables with
/// a statement of what each number covers.
pub fn render_perf(report: &PerfReport) -> String {
    let mut output = String::from("# SolaScope performance\n\n");
    let mut field = |name: &str, value: String| {
        output.push_str(&format!("{name:<10} {value}\n"));
    };
    field("root", report.root.clone());
    field(
        "index dir",
        format!(
            "{} (temporary; {})",
            report.index_dir,
            if report.index_dir_kept {
                "kept"
            } else {
                "removed after the run"
            }
        ),
    );
    field("embedding", report.models.describe_embedding());
    field("rerank", report.models.describe_rerank());
    field("build", report.environment.describe_build());
    field("host", report.environment.describe_host());

    output.push_str("\n## Indexing\n\n");
    let mut indexing = Table::new(&[
        ("phase", Align::Left),
        ("wall ms", Align::Right),
        ("files", Align::Right),
        ("updated", Align::Right),
        ("unchanged", Align::Right),
        ("chunks", Align::Right),
        ("embedded", Align::Right),
        ("cache hits", Align::Right),
        ("index size", Align::Right),
    ]);
    for phase in &report.index_phases {
        indexing.row([
            phase.phase.label().to_owned(),
            table::millis(Some(phase.wall_ms)),
            phase.report.files_total.to_string(),
            phase.report.files_updated.to_string(),
            phase.report.files_unchanged.to_string(),
            phase.report.chunks.to_string(),
            phase.report.embedded_chunks.to_string(),
            phase.report.cache_hits.to_string(),
            table::bytes(phase.index_bytes),
        ]);
    }
    output.push_str(&indexing.render());
    output.push_str(
        "\nwall ms is one Engine::index call in this process: locking and opening the index, scanning\n\
         and hashing sources, parsing, embedding requests and the commit. embedded counts chunks sent\n\
         to the embedding provider, cache hits chunks whose vector was reused. index size is the\n\
         total length of the files in the index directory after the phase.\n",
    );

    output.push_str("\n## Peak resident memory\n\n");
    if report.memory.available {
        let mut memory = Table::new(&[("measured after", Align::Left), ("peak RSS", Align::Right)]);
        memory.row([
            "start (before indexing)".to_owned(),
            table::kib(report.memory.start_peak_rss_kib),
        ]);
        for phase in &report.index_phases {
            memory.row([
                phase.phase.label().to_owned(),
                table::kib(phase.peak_rss_kib),
            ]);
        }
        if let Some(latency) = &report.latency {
            memory.row([
                "search latency".to_owned(),
                table::kib(latency.peak_rss_kib),
            ]);
        }
        output.push_str(&memory.render());
        output.push_str(
            "\nVmHWM from /proc/self/status, the high-water mark of this process. It never decreases:\n\
             each row is the maximum up to that point, not the peak of that phase alone.\n",
        );
    } else {
        output.push_str(&format!(
            "unavailable: {}\n",
            report
                .memory
                .unavailable_reason
                .as_deref()
                .unwrap_or("no reason recorded")
        ));
    }

    output.push_str("\n## Search latency\n\n");
    match &report.latency {
        None => output.push_str(&format!(
            "skipped: {}\n",
            report
                .latency_skipped
                .as_deref()
                .unwrap_or("no reason recorded")
        )),
        Some(latency) => render_latency(&mut output, latency),
    }

    if !report.notices.is_empty() {
        output.push_str("\n## Notices\n\n");
        for notice in &report.notices {
            output.push_str(&format!("- {notice}\n"));
        }
    }
    output
}

fn render_latency(output: &mut String, latency: &LatencyReport) {
    output.push_str(&format!(
        "Timings are {}.\n{} queries, limit {}; per query and variant: 1 first run, then {} measured runs.\n",
        latency.scope, latency.query_count, latency.limit, latency.runs
    ));
    let mut undersampled = false;
    let mut percentile = |value: Option<f64>, flagged: bool| {
        undersampled |= value.is_some() && flagged;
        table::percentile(value, flagged)
    };

    let mut pooled = Table::new(&[
        ("variant", Align::Left),
        ("queries", Align::Right),
        ("first p50 ms", Align::Right),
        ("first max ms", Align::Right),
        ("samples", Align::Right),
        ("min ms", Align::Right),
        ("p50 ms", Align::Right),
        ("p95 ms", Align::Right),
        ("max ms", Align::Right),
    ]);
    let mut by_query = Table::new(&[
        ("variant", Align::Left),
        ("query", Align::Left),
        ("results", Align::Right),
        ("first ms", Align::Right),
        ("min ms", Align::Right),
        ("p50 ms", Align::Right),
        ("p95 ms", Align::Right),
        ("max ms", Align::Right),
    ]);
    for variant in &latency.variants {
        let (first, repeated) = (&variant.first_run, &variant.repeated);
        pooled.row([
            variant.variant.to_string(),
            first.samples.to_string(),
            percentile(first.p50_ms, first.p50_undersampled),
            table::millis(first.max_ms),
            repeated.samples.to_string(),
            table::millis(repeated.min_ms),
            percentile(repeated.p50_ms, repeated.p50_undersampled),
            percentile(repeated.p95_ms, repeated.p95_undersampled),
            table::millis(repeated.max_ms),
        ]);
        for row in &variant.queries {
            let repeated = &row.repeated;
            by_query.row([
                variant.variant.to_string(),
                table::shorten(&row.query, 60),
                row.result_count
                    .map_or_else(|| "-".to_owned(), |count| count.to_string()),
                table::millis(row.first_ms),
                table::millis(repeated.min_ms),
                percentile(repeated.p50_ms, repeated.p50_undersampled),
                percentile(repeated.p95_ms, repeated.p95_undersampled),
                table::millis(repeated.max_ms),
            ]);
        }
    }
    output.push_str("\n### By variant (all queries pooled)\n\n");
    output.push_str(&pooled.render());
    output.push_str(
        "\nfirst: the first execution of each query by a fresh engine, summarized across queries.\n\
         min, p50, p95, max: the measured runs that follow it, pooled over all queries; percentiles\n\
         by nearest rank.\n",
    );
    output.push_str("\n### By query\n\n");
    output.push_str(&by_query.render());
    if undersampled {
        output.push_str(
            "\n* too few samples for this percentile: with fewer than 20 the nearest-rank P95 is the\n\
             \x20 maximum, with fewer than 2 the P50 is the only sample.\n",
        );
    }

    if !latency.skipped_variants.is_empty() {
        output.push_str("\nSkipped variants:\n");
        for skip in &latency.skipped_variants {
            output.push_str(&format!("- {}: {}\n", skip.variant, skip.reason));
        }
    }
    push_messages(output, "Search errors", latency, |variant| {
        variant.errors.as_slice()
    });
    push_messages(output, "Engine warnings", latency, |variant| {
        variant.warnings.as_slice()
    });
}

fn push_messages(
    output: &mut String,
    title: &str,
    latency: &LatencyReport,
    select: impl Fn(&VariantLatency) -> &[MessageCount],
) {
    let mut lines = String::new();
    for variant in &latency.variants {
        for entry in select(variant) {
            lines.push_str(&format!(
                "- {}: {} of {} queries: {}\n",
                variant.variant, entry.queries, latency.query_count, entry.message
            ));
        }
    }
    if !lines.is_empty() {
        output.push_str(&format!("\n{title}:\n{lines}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_high_water_mark_from_proc_status() {
        let status = "Name:\tsolascope-bench\nVmPeak:\t  123456 kB\nVmSize:\t  120000 kB\n\
                      VmHWM:\t    6524 kB\nVmRSS:\t    6000 kB\nThreads:\t4\n";
        assert_eq!(parse_vm_hwm_kib(status), Some(6524));
        assert_eq!(parse_vm_hwm_kib("VmHWM: 1 kB"), Some(1));
        for unusable in [
            "",
            "VmRSS:\t 6000 kB\n",
            "VmHWM:\t kB\n",
            "VmHWM:\t 12 MB\n",
            "VmHWM:\t -4 kB\n",
            "VmHWM:\t 12\n",
            "VmHWM:\t 12 kB extra\n",
            "XVmHWM:\t 12 kB\n",
        ] {
            assert_eq!(parse_vm_hwm_kib(unusable), None, "{unusable:?}");
        }
    }

    #[test]
    fn peak_memory_is_available_exactly_on_linux() {
        let peak = peak_rss_kib();
        assert_eq!(peak.is_ok(), cfg!(target_os = "linux"), "{peak:?}");
        match peak {
            Ok(peak) => assert!(peak > 0),
            Err(reason) => assert!(reason.contains("/proc/self/status"), "{reason}"),
        }
    }

    #[test]
    fn glob_metacharacters_are_neutralized() {
        assert_eq!(escape_glob("local-index"), "local-index");
        assert_eq!(escape_glob("a*b?c"), "a[*]b[?]c");
        assert_eq!(escape_glob("[x]{y}"), "[[]x[]][{]y[}]");
    }

    #[test]
    fn only_an_index_inside_the_root_needs_an_exclusion() {
        let root = Path::new("/work/project");
        assert_eq!(
            index_exclusion(root, Path::new("/work/project/.solascope")).as_deref(),
            Some(".solascope")
        );
        assert_eq!(
            index_exclusion(root, Path::new("/work/project/var/index[1]")).as_deref(),
            Some("var/index[[]1[]]")
        );
        assert_eq!(
            index_exclusion(root, Path::new("/work/cache/project")),
            None
        );
        assert_eq!(
            index_exclusion(root, Path::new("/work/project-index")),
            None
        );
        assert_eq!(index_exclusion(root, root), None);
    }

    #[test]
    fn index_files_are_listed_recursively_with_their_sizes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("index.sqlite3"), [0u8; 4096]).unwrap();
        fs::write(directory.path().join("writer.lock"), b"").unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(directory.path().join("nested/extra"), b"12345").unwrap();
        let files = index_files(directory.path()).unwrap();
        let listed: Vec<(String, u64)> = files
            .iter()
            .map(|file| (file.name.replace('\\', "/"), file.bytes))
            .collect();
        assert_eq!(
            listed,
            [
                ("index.sqlite3".to_owned(), 4096),
                ("nested/extra".to_owned(), 5),
                ("writer.lock".to_owned(), 0),
            ]
        );
        assert!(index_files(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn phases_are_reported_in_execution_order() {
        let labels: Vec<&str> = Phase::ALL.iter().map(|phase| phase.label()).collect();
        assert_eq!(labels, ["first index", "no-op index", "rebuild"]);
        assert_eq!(
            serde_json::to_value(Phase::ALL).unwrap(),
            serde_json::json!(["first_index", "noop_index", "rebuild"])
        );
        assert!(Phase::Rebuild.rebuild());
        assert!(!Phase::FirstIndex.rebuild() && !Phase::NoopIndex.rebuild());
    }
}
