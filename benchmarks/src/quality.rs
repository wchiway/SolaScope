//! Retrieval quality on an annotated query set, once per available variant.

use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;
use solascope_config::Config;
use solascope_core::{IndexReport, IndexStatus, Result, SearchResult};
use solascope_engine::Engine;

use crate::metrics::{
    judge, rank_metrics, summarize, Expectation, Judgement, Location, RankMetrics, TimingSummary,
    TopOutcome, MRR_CUTOFF, RECALL_CUTOFF,
};
use crate::queries::{AnnotatedQuery, QuerySet};
use crate::report::{
    config_json, millis, tally, Environment, MessageCount, ModelSummary, DEBUG_BUILD_NOTICE,
};
use crate::table::{self, Align, Table};
use crate::variants::{plan, SkippedVariant, Variant};
use crate::Progress;

/// Label of the row that aggregates every category.
pub const ALL_CATEGORIES: &str = "all";

#[derive(Debug, Clone)]
pub struct QualityOptions {
    /// Results requested per query (`SearchRequest::limit`).
    pub limit: usize,
    /// Bring the project's index up to date first; `false` evaluates the
    /// existing index as it is.
    pub index: bool,
    /// Where the annotations came from, for the report.
    pub queries_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualityReport {
    pub benchmark: &'static str,
    pub environment: Environment,
    pub root: String,
    pub queries_file: Option<String>,
    pub query_count: usize,
    pub categories: Vec<String>,
    pub limit: usize,
    pub models: ModelSummary,
    pub index: IndexSummary,
    pub variants: Vec<VariantQuality>,
    pub skipped_variants: Vec<SkippedVariant>,
    pub notices: Vec<String>,
    /// The effective configuration the engine ran with.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    /// Whether this run updated the index before searching.
    pub indexed: bool,
    /// Wall time of that update, around `Engine::index`.
    pub wall_ms: Option<f64>,
    pub report: Option<IndexReport>,
    /// The index the queries ran against.
    pub status: IndexStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantQuality {
    pub variant: Variant,
    pub overall: GroupQuality,
    pub categories: Vec<GroupQuality>,
    /// Queries whose first result is not the expected location, in file order.
    pub misses: Vec<QueryOutcome>,
    /// Distinct search errors and how many queries hit each.
    pub errors: Vec<MessageCount>,
    /// Distinct engine warnings and how many queries produced each. A warning
    /// here usually means the variant ran in a degraded form.
    pub warnings: Vec<MessageCount>,
    /// Every query, in file order.
    pub outcomes: Vec<QueryOutcome>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupQuality {
    pub category: String,
    #[serde(flatten)]
    pub metrics: RankMetrics,
    /// Wall time of the successful searches, one execution per query.
    pub time_ms: TimingSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryOutcome {
    pub category: String,
    pub query: String,
    pub expected: Expectation,
    /// `None` when the search failed; see `error`.
    pub judgement: Option<Judgement>,
    pub result_count: usize,
    pub top_result: Option<Location>,
    /// Wall time around the single `Engine::search` call for this query.
    pub elapsed_ms: f64,
    pub error: Option<String>,
}

impl QueryOutcome {
    /// Anything but the expected location in first place.
    pub fn is_miss(&self) -> bool {
        !matches!(
            self.judgement,
            Some(Judgement {
                top: TopOutcome::Expected,
                ..
            })
        )
    }
}

fn location(result: &SearchResult) -> Location {
    Location {
        path: result.relative_path.clone(),
        symbol: result.symbol.clone(),
        start_line: result.start_line,
        end_line: result.end_line,
    }
}

fn group(category: &str, outcomes: &[&QueryOutcome]) -> GroupQuality {
    let samples: Vec<f64> = outcomes
        .iter()
        .filter(|outcome| outcome.error.is_none())
        .map(|outcome| outcome.elapsed_ms)
        .collect();
    GroupQuality {
        category: category.to_owned(),
        metrics: rank_metrics(outcomes.iter().map(|outcome| outcome.judgement)),
        time_ms: summarize(&samples),
    }
}

fn summarize_variant(
    variant: Variant,
    categories: &[String],
    outcomes: Vec<QueryOutcome>,
    warnings: Vec<MessageCount>,
) -> VariantQuality {
    let everything: Vec<&QueryOutcome> = outcomes.iter().collect();
    let per_category = categories
        .iter()
        .map(|category| {
            let members: Vec<&QueryOutcome> = outcomes
                .iter()
                .filter(|outcome| &outcome.category == category)
                .collect();
            group(category, &members)
        })
        .collect();
    VariantQuality {
        variant,
        overall: group(ALL_CATEGORIES, &everything),
        categories: per_category,
        misses: outcomes
            .iter()
            .filter(|outcome| outcome.is_miss())
            .cloned()
            .collect(),
        errors: tally(
            outcomes
                .iter()
                .filter_map(|outcome| outcome.error.as_deref()),
        ),
        warnings,
        outcomes,
    }
}

async fn run_variant(
    engine: &Engine,
    variant: Variant,
    queries: &[AnnotatedQuery],
    limit: usize,
) -> (Vec<QueryOutcome>, Vec<MessageCount>) {
    let mut outcomes = Vec::with_capacity(queries.len());
    let mut warnings: Vec<String> = Vec::new();
    for item in queries {
        let request = variant.request(&item.query, limit);
        let started = Instant::now();
        let result = engine.search(request).await;
        let elapsed_ms = millis(started.elapsed());
        let mut outcome = QueryOutcome {
            category: item.category.clone(),
            query: item.query.clone(),
            expected: item.expected.clone(),
            judgement: None,
            result_count: 0,
            top_result: None,
            elapsed_ms,
            error: None,
        };
        match result {
            Ok(response) => {
                let locations: Vec<Location> = response.results.iter().map(location).collect();
                outcome.judgement = Some(judge(&item.expected, &locations));
                outcome.result_count = locations.len();
                outcome.top_result = locations.into_iter().next();
                warnings.extend(response.warnings);
            }
            // A failed search is a result of the evaluation, not a reason to stop.
            Err(error) => outcome.error = Some(error.to_string()),
        }
        outcomes.push(outcome);
    }
    (outcomes, tally(warnings.iter().map(String::as_str)))
}

/// Runs the quality evaluation against the project described by `config`.
///
/// Unless `options.index` is off, the project's own index is brought up to
/// date first, exactly as `solascope index` would. Each variant then gets a
/// fresh [`Engine`], so no in-process cache carries over between variants.
pub async fn run_quality(
    config: Config,
    queries: &QuerySet,
    options: &QualityOptions,
    progress: Progress<'_>,
) -> Result<QualityReport> {
    let environment = Environment::detect();
    let mut config = config;
    let annotation_notice = options
        .queries_file
        .as_deref()
        .and_then(|file| crate::queries::exclude_query_file(&mut config, file));
    let engine = Engine::new(config)?;
    let config = engine.config().clone();

    let mut notices = Vec::new();
    if environment.is_debug_build() {
        notices.push(DEBUG_BUILD_NOTICE.to_owned());
    }
    if options.limit < MRR_CUTOFF {
        notices.push(format!(
            "limit {} is below {MRR_CUTOFF}: Recall@{RECALL_CUTOFF} and MRR@{MRR_CUTOFF} only see the first {} result(s) of each query",
            options.limit, options.limit
        ));
    }
    if !queries.unknown_fields.is_empty() {
        notices.push(format!(
            "ignored unknown field(s) in the query file: {}",
            queries.unknown_fields.join(", ")
        ));
    }
    if let Some(notice) = annotation_notice {
        notices.push(notice);
    }
    for notice in &notices {
        progress(&format!("notice: {notice}"));
    }

    let index = if options.index {
        progress(&format!(
            "quality: updating the index of {}",
            config.project.root.display()
        ));
        let started = Instant::now();
        let report = engine.index(false).await?;
        let wall_ms = millis(started.elapsed());
        for warning in &report.warnings {
            progress(&format!("warning: {warning}"));
        }
        progress(&format!(
            "quality: indexed {} file(s) ({} updated, {} unchanged, {} deleted), {} chunk(s) in {} ms",
            report.files_total,
            report.files_updated,
            report.files_unchanged,
            report.files_deleted,
            report.chunks,
            table::millis(Some(wall_ms))
        ));
        IndexSummary {
            indexed: true,
            wall_ms: Some(wall_ms),
            report: Some(report),
            status: engine.status()?,
        }
    } else {
        progress("quality: using the existing index as it is (--no-index)");
        IndexSummary {
            indexed: false,
            wall_ms: None,
            report: None,
            status: engine.status()?,
        }
    };

    let categories = queries.categories();
    let plan = plan(&config);
    for skip in &plan.skipped {
        progress(&format!(
            "quality: skipping {}: {}",
            skip.variant, skip.reason
        ));
    }
    let mut variants = Vec::with_capacity(plan.run.len());
    for variant in plan.run {
        progress(&format!(
            "quality: {variant}: running {} queries",
            queries.queries.len()
        ));
        let engine = Engine::new(config.clone())?;
        let (outcomes, warnings) =
            run_variant(&engine, variant, &queries.queries, options.limit).await;
        let summary = summarize_variant(variant, &categories, outcomes, warnings);
        for error in &summary.errors {
            progress(&format!(
                "quality: {variant}: {} of {} searches failed: {}",
                error.queries, summary.overall.metrics.queries, error.message
            ));
        }
        variants.push(summary);
    }

    Ok(QualityReport {
        benchmark: "quality",
        environment,
        root: config.project.root.display().to_string(),
        queries_file: options
            .queries_file
            .as_ref()
            .map(|file| file.display().to_string()),
        query_count: queries.queries.len(),
        categories,
        limit: options.limit,
        models: ModelSummary::from_config(&config),
        index,
        variants,
        skipped_variants: plan.skipped,
        notices,
        config: config_json(&config),
    })
}

fn describe_location(path: &str, lines: Option<(usize, usize)>, symbol: Option<&str>) -> String {
    let symbol = symbol.unwrap_or("(no symbol)");
    match lines {
        Some((start, end)) => format!("{path}:{start}-{end} {symbol}"),
        None => format!("{path} {symbol}"),
    }
}

fn describe_miss(outcome: &QueryOutcome) -> String {
    if let Some(error) = &outcome.error {
        return format!("error: {error}");
    }
    match outcome.judgement {
        Some(Judgement {
            top: TopOutcome::NoResults,
            ..
        }) => "no results".to_owned(),
        Some(Judgement {
            top: TopOutcome::WrongLines,
            ..
        }) => "rank 1, but not the annotated lines".to_owned(),
        Some(Judgement {
            rank: Some(rank), ..
        }) => format!("rank {rank}"),
        _ => format!("not among the {} result(s)", outcome.result_count),
    }
}

/// Renders the report as text: a header, the metrics table, skipped variants,
/// engine warnings, the misses of every variant and explanatory notes.
pub fn render_quality(report: &QualityReport) -> String {
    let mut output = String::from("# SolaScope retrieval quality\n\n");
    let mut field = |name: &str, value: String| {
        output.push_str(&format!("{name:<10} {value}\n"));
    };
    field("root", report.root.clone());
    field(
        "queries",
        format!(
            "{} in {} categor{}{}",
            report.query_count,
            report.categories.len(),
            if report.categories.len() == 1 {
                "y"
            } else {
                "ies"
            },
            report
                .queries_file
                .as_ref()
                .map(|file| format!(" ({file})"))
                .unwrap_or_default()
        ),
    );
    let status = &report.index.status;
    field(
        "index",
        format!(
            "generation {}: {} files, {} chunks, {} vectors{}",
            status.index_version,
            status.files,
            status.chunks,
            status.vectors,
            match (&report.index.report, report.index.wall_ms) {
                (Some(indexed), Some(wall_ms)) => format!(
                    " (updated by this run in {} ms: {} updated, {} unchanged, {} deleted)",
                    table::millis(Some(wall_ms)),
                    indexed.files_updated,
                    indexed.files_unchanged,
                    indexed.files_deleted
                ),
                _ => " (existing index, not updated by this run)".to_owned(),
            }
        ),
    );
    field("embedding", report.models.describe_embedding());
    field("rerank", report.models.describe_rerank());
    field("limit", report.limit.to_string());
    field("build", report.environment.describe_build());
    field("host", report.environment.describe_host());

    output.push_str("\n## Metrics\n\n");
    let mut metrics = Table::new(&[
        ("variant", Align::Left),
        ("category", Align::Left),
        ("queries", Align::Right),
        ("top1", Align::Right),
        ("recall@5", Align::Right),
        ("mrr@10", Align::Right),
        ("wrong-loc", Align::Right),
        ("no-result", Align::Right),
        ("errors", Align::Right),
        ("mean ms", Align::Right),
        ("p50 ms", Align::Right),
        ("p95 ms", Align::Right),
    ]);
    let mut undersampled = false;
    for variant in &report.variants {
        for group in variant
            .categories
            .iter()
            .chain(std::iter::once(&variant.overall))
        {
            let time = &group.time_ms;
            undersampled |= (time.p50_ms.is_some() && time.p50_undersampled)
                || (time.p95_ms.is_some() && time.p95_undersampled);
            metrics.row([
                variant.variant.to_string(),
                group.category.clone(),
                group.metrics.queries.to_string(),
                table::rate(group.metrics.top1),
                table::rate(group.metrics.recall_at_5),
                table::rate(group.metrics.mrr_at_10),
                table::rate(group.metrics.wrong_location_rate),
                table::rate(group.metrics.no_result_rate),
                group.metrics.errors.to_string(),
                table::millis(time.mean_ms),
                table::percentile(time.p50_ms, time.p50_undersampled),
                table::percentile(time.p95_ms, time.p95_undersampled),
            ]);
        }
    }
    output.push_str(&metrics.render());
    output.push_str(
        "\nRates are fractions of the row's queries. top1, recall@5 and mrr@10 match on path and\n\
         symbol; wrong-loc counts queries whose first result is another location, or the right\n\
         symbol at other lines than annotated. Times are one in-process Engine::search call per\n\
         query (successful searches only), percentiles by nearest rank.\n",
    );
    if undersampled {
        output.push_str(
            "* too few samples for this percentile: with fewer than 20 the nearest-rank P95 is the\n\
             \x20 slowest query, with fewer than 2 the P50 is the only one.\n",
        );
    }

    if !report.skipped_variants.is_empty() {
        output.push_str("\n## Skipped variants\n\n");
        for skip in &report.skipped_variants {
            output.push_str(&format!("- {}: {}\n", skip.variant, skip.reason));
        }
    }

    if report
        .variants
        .iter()
        .any(|variant| !variant.warnings.is_empty())
    {
        output.push_str("\n## Engine warnings\n\n");
        for variant in &report.variants {
            for warning in &variant.warnings {
                output.push_str(&format!(
                    "- {}: {} of {} queries: {}\n",
                    variant.variant,
                    warning.queries,
                    variant.overall.metrics.queries,
                    warning.message
                ));
            }
        }
    }

    output.push_str("\n## Misses (expected location not ranked first)\n");
    for variant in &report.variants {
        output.push_str(&format!("\n### {}\n\n", variant.variant));
        if variant.misses.is_empty() {
            output.push_str("none\n");
            continue;
        }
        let mut misses = Table::new(&[
            ("category", Align::Left),
            ("query", Align::Left),
            ("expected", Align::Left),
            ("outcome", Align::Left),
            ("first result", Align::Left),
        ]);
        for miss in &variant.misses {
            misses.row([
                miss.category.clone(),
                table::shorten(&miss.query, 60),
                describe_location(
                    &miss.expected.path,
                    miss.expected.lines.map(|lines| (lines.start, lines.end)),
                    Some(&miss.expected.symbol),
                ),
                table::shorten(&describe_miss(miss), 100),
                miss.top_result.as_ref().map_or_else(
                    || "-".to_owned(),
                    |top| {
                        describe_location(
                            &top.path,
                            Some((top.start_line, top.end_line)),
                            top.symbol.as_deref(),
                        )
                    },
                ),
            ]);
        }
        output.push_str(&misses.render());
    }

    if !report.notices.is_empty() {
        output.push_str("\n## Notices\n\n");
        for notice in &report.notices {
            output.push_str(&format!("- {notice}\n"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::LineRange;

    fn outcome(category: &str, judgement: Option<Judgement>, elapsed_ms: f64) -> QueryOutcome {
        QueryOutcome {
            category: category.into(),
            query: format!("query in {category}"),
            expected: Expectation {
                path: "a.rs".into(),
                symbol: "target".into(),
                lines: Some(LineRange { start: 6, end: 12 }),
            },
            judgement,
            result_count: usize::from(judgement.is_some_and(|j| j.top != TopOutcome::NoResults)),
            top_result: None,
            elapsed_ms,
            error: judgement
                .is_none()
                .then(|| "model unavailable: down".to_owned()),
        }
    }

    fn judged(rank: Option<usize>, top: TopOutcome) -> Option<Judgement> {
        Some(Judgement { rank, top })
    }

    #[test]
    fn variants_are_summarized_overall_and_per_category() {
        let outcomes = vec![
            outcome("symbol", judged(Some(1), TopOutcome::Expected), 1.0),
            outcome("symbol", judged(Some(2), TopOutcome::WrongLocation), 3.0),
            outcome("natural", judged(None, TopOutcome::NoResults), 5.0),
            outcome("natural", None, 900.0),
        ];
        let categories = vec!["symbol".to_owned(), "natural".to_owned()];
        let summary = summarize_variant(Variant::Hybrid, &categories, outcomes, Vec::new());

        assert_eq!(summary.overall.category, ALL_CATEGORIES);
        assert_eq!(summary.overall.metrics.queries, 4);
        assert_eq!(summary.overall.metrics.errors, 1);
        assert_eq!(summary.overall.metrics.top1, 0.25);
        assert_eq!(summary.overall.metrics.recall_at_5, 0.5);
        assert_eq!(summary.overall.metrics.mrr_at_10, 0.375);
        assert_eq!(summary.overall.metrics.wrong_location_rate, 0.25);
        assert_eq!(summary.overall.metrics.no_result_rate, 0.25);
        // The failed search does not contribute a timing sample.
        assert_eq!(summary.overall.time_ms.samples, 3);
        assert_eq!(summary.overall.time_ms.max_ms, Some(5.0));

        let names: Vec<&str> = summary
            .categories
            .iter()
            .map(|group| group.category.as_str())
            .collect();
        assert_eq!(names, ["symbol", "natural"]);
        assert_eq!(summary.categories[0].metrics.top1, 0.5);
        assert_eq!(summary.categories[1].metrics.errors, 1);
        assert_eq!(summary.categories[1].time_ms.samples, 1);

        assert_eq!(summary.misses.len(), 3);
        assert_eq!(summary.outcomes.len(), 4);
        assert_eq!(
            summary.errors,
            [MessageCount {
                message: "model unavailable: down".into(),
                queries: 1
            }]
        );
    }

    #[test]
    fn misses_are_described_by_what_went_wrong() {
        let describe = |judgement, results| {
            let mut outcome = outcome("c", judgement, 1.0);
            outcome.result_count = results;
            describe_miss(&outcome)
        };
        assert_eq!(
            describe(judged(Some(3), TopOutcome::WrongLocation), 10),
            "rank 3"
        );
        assert_eq!(
            describe(judged(None, TopOutcome::WrongLocation), 7),
            "not among the 7 result(s)"
        );
        assert_eq!(
            describe(judged(None, TopOutcome::NoResults), 0),
            "no results"
        );
        assert_eq!(
            describe(judged(Some(1), TopOutcome::WrongLines), 4),
            "rank 1, but not the annotated lines"
        );
        assert_eq!(describe(None, 0), "error: model unavailable: down");
        assert!(!outcome("c", judged(Some(1), TopOutcome::Expected), 1.0).is_miss());
        assert!(outcome("c", judged(Some(1), TopOutcome::WrongLines), 1.0).is_miss());
        assert!(outcome("c", None, 1.0).is_miss());
    }

    #[test]
    fn locations_are_written_like_search_results() {
        assert_eq!(
            describe_location("a.rs", Some((6, 12)), Some("Auth::login")),
            "a.rs:6-12 Auth::login"
        );
        assert_eq!(describe_location("a.rs", None, Some("login")), "a.rs login");
        assert_eq!(
            describe_location("notes.txt", Some((1, 40)), None),
            "notes.txt:1-40 (no symbol)"
        );
    }
}
