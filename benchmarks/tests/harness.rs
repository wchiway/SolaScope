//! Both subcommands' core functions against a copy of the annotated
//! multi-language fixture, without any model. Assertions cover the structure of
//! the reports and the fixture's known lexical quality, never timings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use solascope_bench::metrics::TopOutcome;
use solascope_bench::perf::{render_perf, run_perf, PerfOptions, Phase};
use solascope_bench::quality::{
    render_quality, run_quality, QualityOptions, QualityReport, ALL_CATEGORIES,
};
use solascope_bench::queries::{load_annotated, load_query_strings, parse_annotated, QuerySet};
use solascope_bench::variants::Variant;
use solascope_config::Config;
use solascope_core::Error;
use solascope_engine::Engine;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/multilang")
}

fn fixture_queries() -> PathBuf {
    fixture_dir().join("queries.json")
}

/// The fixture sources without `queries.json`: the annotations name every
/// expected answer and are not source code.
fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(fixture_dir()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "queries.json" {
            std::fs::copy(entry.path(), root.path().join(entry.file_name())).unwrap();
        }
    }
    root
}

/// Every directory and file below `root` with its content, to prove that a
/// run left the project exactly as it found it.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                entries.insert(relative, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

/// A snapshot without SQLite's `-wal`/`-shm` companions, which any reader of a
/// WAL-mode database may create next to it.
fn snapshot_without_sidecars(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = snapshot(root);
    entries.retain(|path, _| {
        let name = path.to_string_lossy();
        !name.ends_with("-wal") && !name.ends_with("-shm")
    });
    entries
}

fn options(index: bool) -> QualityOptions {
    QualityOptions {
        limit: 10,
        index,
        queries_file: Some(fixture_queries()),
    }
}

async fn quality(
    config: Config,
    queries: &QuerySet,
    options: &QualityOptions,
) -> (QualityReport, Vec<String>) {
    let mut progress = Vec::new();
    let report = run_quality(config, queries, options, &mut |line: &str| {
        progress.push(line.to_owned())
    })
    .await
    .unwrap();
    (report, progress)
}

fn variants(report: &QualityReport) -> Vec<Variant> {
    report
        .variants
        .iter()
        .map(|variant| variant.variant)
        .collect()
}

#[tokio::test]
async fn quality_reports_perfect_lexical_metrics_for_the_fixture() {
    let root = project();
    let queries = load_annotated(&fixture_queries()).unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let (report, progress) = quality(config, &queries, &options(true)).await;

    assert_eq!(report.benchmark, "quality");
    assert_eq!(report.query_count, 18);
    assert_eq!(
        report.categories,
        ["symbol", "error", "natural", "distractor"]
    );
    assert_eq!(report.limit, 10);
    assert_eq!(report.models.embedding_provider, "disabled");
    assert_eq!(report.models.rerank_provider, "disabled");

    assert!(report.index.indexed);
    assert!(report.index.wall_ms.is_some());
    let indexed = report.index.report.as_ref().unwrap();
    assert_eq!((indexed.files_total, indexed.files_updated), (5, 5));
    assert_eq!(report.index.status.files, 5);
    assert!(report.index.status.chunks > 0);
    assert_eq!(report.index.status.vectors, 0);

    // Without models only two variants can run; the others are named with a reason.
    assert_eq!(variants(&report), [Variant::Lexical, Variant::Hybrid]);
    let skipped: Vec<Variant> = report
        .skipped_variants
        .iter()
        .map(|skip| skip.variant)
        .collect();
    assert_eq!(skipped, [Variant::Semantic, Variant::HybridRerank]);
    assert!(report.skipped_variants[0]
        .reason
        .contains("embedding.provider"));
    assert!(report.skipped_variants[1]
        .reason
        .contains("rerank.provider"));
    for name in ["semantic", "hybrid+rerank"] {
        assert!(
            progress
                .iter()
                .any(|line| line.contains(&format!("skipping {name}: "))),
            "{progress:?}"
        );
    }

    let lexical = &report.variants[0];
    let sizes: Vec<(&str, usize)> = lexical
        .categories
        .iter()
        .map(|group| (group.category.as_str(), group.metrics.queries))
        .collect();
    assert_eq!(
        sizes,
        [
            ("symbol", 5),
            ("error", 5),
            ("natural", 6),
            ("distractor", 2)
        ]
    );
    assert_eq!(lexical.overall.category, ALL_CATEGORIES);
    assert_eq!(lexical.overall.metrics.queries, 18);
    for group in lexical.categories.iter().chain([&lexical.overall]) {
        let metrics = &group.metrics;
        assert_eq!(metrics.top1, 1.0, "{}", group.category);
        assert_eq!(metrics.recall_at_5, 1.0, "{}", group.category);
        assert_eq!(metrics.mrr_at_10, 1.0, "{}", group.category);
        assert_eq!(metrics.wrong_location_rate, 0.0, "{}", group.category);
        assert_eq!(metrics.no_result_rate, 0.0, "{}", group.category);
        assert_eq!(metrics.errors, 0, "{}", group.category);
        assert_eq!(group.time_ms.samples, metrics.queries);
        assert!(group.time_ms.mean_ms.is_some());
    }
    assert!(lexical.misses.is_empty());
    assert!(lexical.errors.is_empty());
    assert!(lexical.warnings.is_empty());
    assert_eq!(lexical.outcomes.len(), 18);
    for outcome in &lexical.outcomes {
        let judgement = outcome.judgement.unwrap();
        assert_eq!(judgement.rank, Some(1), "{}", outcome.query);
        assert_eq!(judgement.top, TopOutcome::Expected, "{}", outcome.query);
        assert!(outcome.result_count >= 1);
        assert_eq!(
            outcome.top_result.as_ref().unwrap().path,
            outcome.expected.path
        );
        assert!(outcome.error.is_none());
    }
    // Eighteen samples cannot resolve a P95: it is the maximum, and flagged.
    let time = &lexical.overall.time_ms;
    assert!(time.p95_undersampled && !time.p50_undersampled);
    assert_eq!(time.p95_ms, time.max_ms);

    // Hybrid ran, but without a vector channel, and the report says so.
    let hybrid = &report.variants[1];
    assert_eq!(hybrid.overall.metrics.queries, 18);
    assert_eq!(hybrid.overall.metrics.errors, 0);
    assert_eq!(hybrid.categories.len(), 4);
    assert_eq!(hybrid.outcomes.len(), 18);
    assert_eq!(hybrid.warnings.len(), 1, "{:?}", hybrid.warnings);
    assert_eq!(hybrid.warnings[0].queries, 18);
    assert!(hybrid.warnings[0].message.contains("embedding is disabled"));
}

#[tokio::test]
async fn quality_renders_tables_and_one_json_document() {
    let root = project();
    let queries = load_annotated(&fixture_queries()).unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let (report, _) = quality(config, &queries, &options(true)).await;

    let text = render_quality(&report);
    for expected in [
        "# SolaScope retrieval quality",
        "## Metrics",
        "| variant | category   | queries |",
        "| lexical | symbol     |       5 | 1.000 |    1.000 |  1.000 |     0.000 |     0.000 |      0 |",
        "| lexical | all        |      18 | 1.000 |    1.000 |  1.000 |     0.000 |     0.000 |      0 |",
        "| hybrid  | all        |      18 |",
        "## Skipped variants",
        "- semantic: embedding.provider is \"disabled\"",
        "- hybrid+rerank: rerank.provider is \"disabled\"",
        "## Engine warnings",
        "- hybrid: 18 of 18 queries: embedding is disabled",
        "## Misses (expected location not ranked first)",
        "### lexical\n\nnone\n",
        "nearest rank",
        "* too few samples",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["benchmark"], "quality");
    assert_eq!(json["variants"][0]["variant"], "lexical");
    assert_eq!(json["variants"][0]["overall"]["category"], "all");
    assert_eq!(json["variants"][0]["overall"]["queries"], 18);
    assert_eq!(json["variants"][0]["overall"]["top1"], 1.0);
    assert_eq!(json["variants"][0]["overall"]["recall_at_5"], 1.0);
    assert_eq!(json["variants"][0]["overall"]["mrr_at_10"], 1.0);
    assert_eq!(json["variants"][0]["overall"]["wrong_location_rate"], 0.0);
    assert_eq!(json["variants"][0]["overall"]["no_result_rate"], 0.0);
    assert_eq!(json["variants"][0]["overall"]["time_ms"]["samples"], 18);
    assert_eq!(json["variants"][0]["categories"][2]["category"], "natural");
    assert_eq!(json["variants"][0]["outcomes"][0]["judgement"]["rank"], 1);
    assert_eq!(
        json["variants"][0]["outcomes"][0]["judgement"]["top"],
        "expected"
    );
    assert_eq!(json["variants"][0]["misses"], serde_json::json!([]));
    assert_eq!(json["skipped_variants"][0]["variant"], "semantic");
    assert_eq!(json["skipped_variants"][1]["variant"], "hybrid+rerank");
    assert_eq!(json["index"]["report"]["files_total"], 5);
    assert_eq!(json["config"]["embedding"]["provider"], "disabled");
    assert_eq!(json["environment"]["os"], std::env::consts::OS);
}

#[tokio::test]
async fn quality_without_indexing_needs_and_reuses_an_existing_index() {
    let root = project();
    let queries = load_annotated(&fixture_queries()).unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let before = snapshot(root.path());

    let missing = run_quality(config.clone(), &queries, &options(false), &mut |_: &str| {}).await;
    assert!(matches!(missing, Err(Error::IndexMissing)));
    assert_eq!(
        snapshot(root.path()),
        before,
        "--no-index must not create an index"
    );

    quality(config.clone(), &queries, &options(true)).await;
    let indexed = snapshot_without_sidecars(root.path());
    let (report, progress) = quality(config, &queries, &options(false)).await;
    assert!(!report.index.indexed);
    assert!(report.index.report.is_none() && report.index.wall_ms.is_none());
    assert_eq!(report.index.status.files, 5);
    assert_eq!(report.variants[0].overall.metrics.top1, 1.0);
    assert!(progress.iter().any(|line| line.contains("--no-index")));
    assert!(render_quality(&report).contains("existing index, not updated by this run"));
    assert_eq!(
        snapshot_without_sidecars(root.path()),
        indexed,
        "--no-index must not change the sources or the index database"
    );
}

#[tokio::test]
async fn quality_tells_wrong_lines_wrong_locations_and_empty_results_apart() {
    let root = project();
    let queries = parse_annotated(
        r#"[
          {"category": "moved", "query": "authenticate_user", "path": "users.rs",
           "symbol": "authenticate_user", "start_line": 1, "end_line": 2},
          {"category": "absent", "query": "calculateShipping", "path": "pricing.js",
           "symbol": "noSuchSymbol"},
          {"category": "empty", "query": "zzqqxxnomatchtoken", "path": "users.rs",
           "symbol": "authenticate_user"},
          {"category": "exact", "query": "find_user", "path": "repository.py",
           "symbol": "find_user", "start_line": 5, "end_line": 9, "note": "kept"}
        ]"#,
    )
    .unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let options = QualityOptions {
        limit: 3,
        index: true,
        queries_file: None,
    };
    let (report, progress) = quality(config, &queries, &options).await;
    let lexical = &report.variants[0];
    assert_eq!(lexical.variant, Variant::Lexical);

    let group = |name: &str| {
        &lexical
            .categories
            .iter()
            .find(|group| group.category == name)
            .unwrap()
            .metrics
    };
    // The right symbol at other lines: found at rank 1, yet a wrong location.
    assert_eq!(group("moved").top1, 1.0);
    assert_eq!(group("moved").wrong_location_rate, 1.0);
    assert_eq!(group("moved").wrong_lines, 1);
    // Results that do not contain the expected symbol.
    assert_eq!(group("absent").top1, 0.0);
    assert_eq!(group("absent").recall_at_5, 0.0);
    assert_eq!(group("absent").wrong_location_rate, 1.0);
    assert_eq!(group("absent").no_result_rate, 0.0);
    // No result at all is not a wrong location.
    assert_eq!(group("empty").no_result_rate, 1.0);
    assert_eq!(group("empty").wrong_location_rate, 0.0);
    assert_eq!(group("exact").top1, 1.0);
    assert_eq!(group("exact").wrong_location_rate, 0.0);

    let overall = &lexical.overall.metrics;
    assert_eq!(overall.queries, 4);
    assert_eq!(
        (overall.top1, overall.recall_at_5, overall.mrr_at_10),
        (0.5, 0.5, 0.5)
    );
    assert_eq!(overall.wrong_location_rate, 0.5);
    assert_eq!(overall.no_result_rate, 0.25);

    let missed: Vec<&str> = lexical
        .misses
        .iter()
        .map(|miss| miss.category.as_str())
        .collect();
    assert_eq!(missed, ["moved", "absent", "empty"]);
    assert_eq!(
        lexical.misses[0].judgement.unwrap().top,
        TopOutcome::WrongLines
    );
    assert_eq!(
        lexical.misses[1]
            .top_result
            .as_ref()
            .unwrap()
            .symbol
            .as_deref(),
        Some("calculateShipping")
    );
    assert_eq!(lexical.misses[2].result_count, 0);
    assert!(lexical.misses[2].top_result.is_none());

    let text = render_quality(&report);
    for expected in [
        "rank 1, but not the annotated lines",
        "not among the",
        "no results",
        "users.rs:1-2 authenticate_user",
        "pricing.js noSuchSymbol",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }

    // Reduced limits and unknown annotation fields are reported, not hidden.
    for expected in [
        "limit 3 is below 10",
        "unknown field(s) in the query file: note",
    ] {
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains(expected)),
            "{:?}",
            report.notices
        );
        assert!(progress.iter().any(|line| line.contains(expected)));
        assert!(text.contains(expected));
    }
}

#[tokio::test]
async fn quality_warns_when_the_annotations_are_inside_the_project() {
    let root = project();
    let inside = root.path().join("queries.json");
    std::fs::copy(fixture_queries(), &inside).unwrap();
    let queries = load_annotated(&inside).unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let options = QualityOptions {
        limit: 10,
        index: true,
        queries_file: Some(inside),
    };
    let (report, _) = quality(config, &queries, &options).await;
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains("inside the project root (queries.json)")),
        "{:?}",
        report.notices
    );
    // Answers must not be included in the indexed corpus.
    assert_eq!(report.index.status.files, 5);
    assert!(report
        .variants
        .iter()
        .all(|variant| variant.overall.metrics.top1 == 1.0));

    let elsewhere = project();
    let (outside, _) = quality(
        Config::default_for_root(elsewhere.path()).unwrap(),
        &queries,
        &self::options(true),
    )
    .await;
    assert_eq!(outside.index.status.files, 5);
    assert!(!outside
        .notices
        .iter()
        .any(|notice| notice.contains("inside the project root")));
}

#[tokio::test]
async fn search_errors_are_reported_per_variant_instead_of_failing_the_run() {
    let root = project();
    let queries = load_annotated(&fixture_queries()).unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    // An index without vectors ...
    Engine::new(config.clone())
        .unwrap()
        .index(false)
        .await
        .unwrap();
    // ... queried with an embedding provider configured. The engine rejects the
    // mismatch before contacting the provider; port 1 keeps even an attempt local.
    let mut with_embedding = config;
    with_embedding.embedding.provider = "ollama".into();
    with_embedding.embedding.endpoint = "http://127.0.0.1:1".into();
    with_embedding.embedding.retries = 0;
    with_embedding.embedding.timeout_secs = 1;
    let (report, progress) = quality(with_embedding, &queries, &options(false)).await;

    assert_eq!(
        variants(&report),
        [Variant::Lexical, Variant::Hybrid, Variant::Semantic]
    );
    assert_eq!(report.skipped_variants.len(), 1);
    assert_eq!(report.skipped_variants[0].variant, Variant::HybridRerank);
    assert_eq!(report.variants[0].overall.metrics.top1, 1.0);
    assert_eq!(report.variants[0].overall.metrics.errors, 0);

    let semantic = &report.variants[2];
    let metrics = &semantic.overall.metrics;
    assert_eq!((metrics.queries, metrics.errors), (18, 18));
    assert_eq!(
        (metrics.top1, metrics.recall_at_5, metrics.mrr_at_10),
        (0.0, 0.0, 0.0)
    );
    // Failures are neither wrong locations nor empty results, and are not timed.
    assert_eq!((metrics.wrong_location, metrics.no_result), (0, 0));
    assert_eq!(semantic.overall.time_ms.samples, 0);
    assert_eq!(semantic.overall.time_ms.p95_ms, None);
    assert_eq!(semantic.misses.len(), 18);
    assert_eq!(
        semantic
            .errors
            .iter()
            .map(|error| error.queries)
            .sum::<usize>(),
        18
    );
    assert!(semantic
        .outcomes
        .iter()
        .all(|outcome| outcome.error.is_some()));
    assert!(progress
        .iter()
        .any(|line| line.contains("semantic") && line.contains("searches failed")));

    let hybrid = &report.variants[1];
    assert_eq!(hybrid.overall.metrics.queries, 18);
    assert_eq!(
        hybrid.overall.time_ms.samples + hybrid.overall.metrics.errors,
        18
    );

    let text = render_quality(&report);
    assert!(
        text.contains("| semantic | all        |      18 | 0.000 |"),
        "{text}"
    );
    assert!(text.contains("error: "), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["variants"][2]["overall"]["errors"], 18);
    assert_eq!(
        json["variants"][2]["overall"]["time_ms"]["p95_ms"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn a_failing_reranker_shows_up_as_engine_warnings() {
    let root = project();
    let queries = load_annotated(&fixture_queries()).unwrap();
    let mut config = Config::default_for_root(root.path()).unwrap();
    config.privacy.remote_opt_in = true;
    config.rerank.provider = "remote".into();
    config.rerank.model = "test".into();
    config.rerank.endpoint = "http://127.0.0.1:1".into();
    // The key is resolved before any request is sent; without it nothing leaves
    // the process and the engine falls back to the fused ranking.
    config.rerank.api_key_env = "SOLASCOPE_BENCH_TEST_UNSET_RERANK_KEY".into();
    config.rerank.timeout_secs = 1;
    let (report, _) = quality(config, &queries, &options(true)).await;

    assert_eq!(
        variants(&report),
        [Variant::Lexical, Variant::Hybrid, Variant::HybridRerank]
    );
    assert_eq!(report.skipped_variants.len(), 1);
    assert_eq!(report.skipped_variants[0].variant, Variant::Semantic);
    assert_eq!(report.models.rerank_model.as_deref(), Some("test"));

    let reranked = &report.variants[2];
    assert_eq!(reranked.overall.metrics.queries, 18);
    assert_eq!(reranked.overall.metrics.errors, 0);
    let fallback = reranked
        .warnings
        .iter()
        .find(|warning| warning.message.contains("reranker unavailable"))
        .unwrap_or_else(|| panic!("{:?}", reranked.warnings));
    assert_eq!(fallback.queries, 18);
    assert!(
        render_quality(&report).contains("- hybrid+rerank: 18 of 18 queries: reranker unavailable")
    );
}

#[tokio::test]
async fn perf_measures_every_phase_and_leaves_the_project_alone() {
    let root = project();
    let config = Config::default_for_root(root.path()).unwrap();
    // The project already has an index of its own; perf must not use it.
    Engine::new(config.clone())
        .unwrap()
        .index(false)
        .await
        .unwrap();
    let before = snapshot(root.path());
    assert!(before.contains_key(Path::new(".solascope/index.sqlite3")));

    let queries = load_query_strings(&fixture_queries()).unwrap();
    assert_eq!((queries.queries.len(), queries.duplicates), (18, 0));
    let mut progress = Vec::new();
    let report = run_perf(
        config,
        Some(&queries),
        &PerfOptions {
            runs: 3,
            keep_index: false,
        },
        &mut |line: &str| progress.push(line.to_owned()),
    )
    .await
    .unwrap();

    assert_eq!(snapshot(root.path()), before, "perf modified the project");
    let index_dir = PathBuf::from(&report.index_dir);
    assert!(!index_dir.exists(), "the temporary index was not removed");
    assert!(!index_dir.starts_with(root.path()));
    assert!(!report.index_dir_kept);
    assert_eq!(report.benchmark, "perf");

    let phases: Vec<Phase> = report
        .index_phases
        .iter()
        .map(|phase| phase.phase)
        .collect();
    assert_eq!(
        phases,
        [Phase::FirstIndex, Phase::NoopIndex, Phase::Rebuild]
    );
    let (first, noop, rebuild) = (
        &report.index_phases[0],
        &report.index_phases[1],
        &report.index_phases[2],
    );
    assert_eq!(first.report.files_total, 5);
    assert_eq!(first.report.files_updated, 5);
    assert!(first.report.chunks > 0);
    assert_eq!(first.report.embedded_chunks, 0);
    assert!(!first.report.rebuilt);
    assert_eq!(noop.report.files_updated, 0);
    assert_eq!(noop.report.files_unchanged, 5);
    assert_eq!(noop.report.chunks, first.report.chunks);
    assert_eq!(noop.report.index_version, first.report.index_version);
    assert!(rebuild.report.rebuilt);
    assert_eq!(rebuild.report.files_updated, 5);
    assert_eq!(rebuild.report.chunks, first.report.chunks);
    for phase in &report.index_phases {
        assert!(phase.index_bytes > 0);
        assert!(phase
            .index_files
            .iter()
            .any(|file| file.name == "index.sqlite3" && file.bytes > 0));
        assert_eq!(
            phase.index_bytes,
            phase.index_files.iter().map(|file| file.bytes).sum::<u64>()
        );
    }

    let latency = report.latency.as_ref().unwrap();
    assert!(report.latency_skipped.is_none());
    assert_eq!(latency.runs, 3);
    assert_eq!(latency.query_count, 18);
    assert_eq!(latency.limit, 10);
    assert!(latency.scope.contains("excludes process start-up"));
    let measured: Vec<Variant> = latency
        .variants
        .iter()
        .map(|variant| variant.variant)
        .collect();
    assert_eq!(measured, [Variant::Lexical, Variant::Hybrid]);
    let skipped: Vec<Variant> = latency
        .skipped_variants
        .iter()
        .map(|skip| skip.variant)
        .collect();
    assert_eq!(skipped, [Variant::Semantic, Variant::HybridRerank]);
    for variant in &latency.variants {
        assert!(variant.errors.is_empty());
        assert_eq!(variant.queries.len(), 18);
        assert_eq!(variant.first_run.samples, 18);
        assert_eq!(variant.repeated.samples, 18 * 3);
        // 54 pooled samples resolve a P95; three per query do not.
        assert!(!variant.repeated.p95_undersampled);
        for row in &variant.queries {
            assert!(row.error.is_none(), "{}", row.query);
            assert!(row.result_count.unwrap() >= 1, "{}", row.query);
            assert!(row.first_ms.is_some());
            assert_eq!(row.repeated.samples, 3);
            assert!(row.repeated.p95_undersampled && !row.repeated.p50_undersampled);
            assert_eq!(row.repeated.p95_ms, row.repeated.max_ms);
            assert!(row.repeated.min_ms <= row.repeated.p50_ms);
            assert!(row.repeated.p50_ms <= row.repeated.max_ms);
        }
    }

    // Peak resident memory: measured on Linux, declared unavailable elsewhere.
    let peaks: Vec<Option<u64>> = std::iter::once(report.memory.start_peak_rss_kib)
        .chain(report.index_phases.iter().map(|phase| phase.peak_rss_kib))
        .chain([latency.peak_rss_kib])
        .collect();
    if cfg!(target_os = "linux") {
        assert!(report.memory.available);
        assert!(report.memory.unavailable_reason.is_none());
        let peaks: Vec<u64> = peaks.into_iter().map(Option::unwrap).collect();
        // /proc accounts RSS approximately while other test threads allocate
        // and release pages. Validate the samples, not strict monotonicity.
        assert!(peaks.iter().all(|peak| *peak > 0), "{peaks:?}");
    } else {
        assert!(!report.memory.available);
        assert!(report.memory.unavailable_reason.is_some());
        assert!(peaks.iter().all(Option::is_none));
    }

    for expected in [
        "temporary index directory",
        "first index",
        "no-op index",
        "rebuild",
        "skipping semantic: ",
        "skipping hybrid+rerank: ",
    ] {
        assert!(
            progress.iter().any(|line| line.contains(expected)),
            "missing {expected:?} in {progress:?}"
        );
    }

    let text = render_perf(&report);
    for expected in [
        "# SolaScope performance",
        "(temporary; removed after the run)",
        "## Indexing",
        "| first index |",
        "| no-op index |",
        "| rebuild     |",
        "## Peak resident memory",
        "## Search latency",
        "excludes process start-up",
        "18 queries, limit 10; per query and variant: 1 first run, then 3 measured runs.",
        "### By variant (all queries pooled)",
        "### By query",
        "| lexical | authenticate_user",
        "Skipped variants:",
        "- semantic: embedding.provider is \"disabled\"",
        "* too few samples",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    if cfg!(target_os = "linux") {
        assert!(text.contains("| start (before indexing) |"), "{text}");
        assert!(text.contains("It never decreases"), "{text}");
    } else {
        assert!(text.contains("unavailable: "), "{text}");
    }

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["benchmark"], "perf");
    assert_eq!(json["index_phases"][0]["phase"], "first_index");
    assert_eq!(json["index_phases"][1]["phase"], "noop_index");
    assert_eq!(json["index_phases"][2]["phase"], "rebuild");
    assert_eq!(json["index_phases"][0]["report"]["files_total"], 5);
    assert_eq!(json["latency"]["runs"], 3);
    assert_eq!(json["latency"]["variants"][0]["variant"], "lexical");
    assert_eq!(json["latency"]["variants"][0]["repeated"]["samples"], 54);
    assert_eq!(
        json["latency"]["variants"][0]["queries"][0]["repeated"]["samples"],
        3
    );
    assert_eq!(json["memory"]["available"], cfg!(target_os = "linux"));
}

/// Removes a kept index directory even when an assertion fails first.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn perf_without_queries_skips_latency_and_can_keep_its_index() {
    let root = project();
    let before = snapshot(root.path());
    let config = Config::default_for_root(root.path()).unwrap();
    let mut progress = Vec::new();
    let report = run_perf(
        config,
        None,
        &PerfOptions {
            runs: 30,
            keep_index: true,
        },
        &mut |line: &str| progress.push(line.to_owned()),
    )
    .await
    .unwrap();
    let kept = RemoveOnDrop(PathBuf::from(&report.index_dir));

    // No index appeared inside the project either.
    assert_eq!(snapshot(root.path()), before);
    assert!(report.index_dir_kept);
    assert!(kept.0.join("index.sqlite3").is_file());
    assert_eq!(report.index_phases.len(), 3);

    assert!(report.latency.is_none());
    let reason = report.latency_skipped.as_deref().unwrap();
    assert!(reason.contains("--queries"), "{reason}");
    assert!(
        progress
            .iter()
            .any(|line| line.contains("skipping search latency") && line.contains("--queries")),
        "{progress:?}"
    );
    let text = render_perf(&report);
    assert!(
        text.contains("## Search latency\n\nskipped: no query file was given"),
        "{text}"
    );
    assert!(text.contains("(temporary; kept)"), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["latency"], serde_json::Value::Null);
    assert_eq!(json["index_dir_kept"], true);
}

#[tokio::test]
async fn perf_scans_the_same_files_when_the_project_keeps_its_index_elsewhere() {
    let root = project();
    let mut config = Config::default_for_root(root.path()).unwrap();
    config.project.index_dir = root.path().join("local-index");
    let own = Engine::new(config.clone())
        .unwrap()
        .index(false)
        .await
        .unwrap();
    assert_eq!(own.files_total, 5);
    let before = snapshot(root.path());
    assert!(before.contains_key(Path::new("local-index/index.sqlite3")));

    let report = run_perf(
        config,
        None,
        &PerfOptions {
            runs: 1,
            keep_index: false,
        },
        &mut |_: &str| {},
    )
    .await
    .unwrap();
    // The project's own index directory is neither changed nor indexed as source.
    assert_eq!(snapshot(root.path()), before);
    let first = &report.index_phases[0].report;
    assert_eq!(first.files_total, own.files_total);
    assert_eq!(first.skipped_files, own.skipped_files);
    assert_eq!(first.chunks, own.chunks);
}

#[tokio::test]
async fn perf_rejects_zero_runs_before_doing_any_work() {
    let root = project();
    let config = Config::default_for_root(root.path()).unwrap();
    let result = run_perf(
        config,
        None,
        &PerfOptions {
            runs: 0,
            keep_index: false,
        },
        &mut |_: &str| {},
    )
    .await;
    assert!(matches!(result, Err(Error::InvalidInput(_))));
}
