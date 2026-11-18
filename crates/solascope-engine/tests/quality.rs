//! Retrieval quality on the annotated multi-language fixture, without any model.
//! Run with `--nocapture` to print the measured metrics.
use solascope_config::Config;
use solascope_core::{SearchMode, SearchRequest};
use solascope_engine::Engine;
use std::path::Path;

#[tokio::test]
async fn annotated_queries_locate_the_expected_definitions() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/multilang");
    let root = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&fixtures).unwrap() {
        let entry = entry.unwrap();
        // The annotations name every expected answer; they are not source code.
        if entry.file_name() == "queries.json" {
            continue;
        }
        std::fs::copy(entry.path(), root.path().join(entry.file_name())).unwrap();
    }
    let queries: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(fixtures.join("queries.json")).unwrap())
            .unwrap();
    let engine = Engine::new(Config::default_for_root(root.path()).unwrap()).unwrap();
    let report = engine.index(false).await.unwrap();
    assert_eq!(report.files_total, 5, "{report:?}");

    for mode in [SearchMode::Lexical, SearchMode::Hybrid] {
        let (mut top1, mut recall5, mut reciprocal) = (0usize, 0usize, 0.0f64);
        let mut misses = Vec::new();
        for item in &queries {
            let response = engine
                .search(SearchRequest {
                    query: item["query"].as_str().unwrap().into(),
                    mode,
                    limit: 10,
                    rerank: false,
                    ..Default::default()
                })
                .await
                .unwrap();
            let expected_symbol = item["symbol"].as_str().unwrap();
            let position = response.results.iter().position(|result| {
                result.relative_path == item["path"].as_str().unwrap()
                    && result.symbol.as_deref().is_some_and(|symbol| {
                        symbol == expected_symbol
                            || symbol.ends_with(&format!(".{expected_symbol}"))
                            || symbol.ends_with(&format!("::{expected_symbol}"))
                    })
            });
            match position {
                Some(index) => {
                    let hit = &response.results[index];
                    assert!(!hit.stale);
                    if let (Some(start), Some(end)) =
                        (item["start_line"].as_u64(), item["end_line"].as_u64())
                    {
                        assert_eq!(
                            (hit.start_line as u64, hit.end_line as u64),
                            (start, end),
                            "{item}"
                        );
                    }
                    top1 += usize::from(index == 0);
                    recall5 += usize::from(index < 5);
                    reciprocal += 1.0 / (index as f64 + 1.0);
                }
                None => misses.push(item["query"].as_str().unwrap().to_owned()),
            }
        }
        let total = queries.len() as f64;
        println!(
            "mode={mode} queries={} top1={:.3} recall@5={:.3} mrr@10={:.3} misses={misses:?}",
            queries.len(),
            top1 as f64 / total,
            recall5 as f64 / total,
            reciprocal / total,
        );
        assert!(
            recall5 as f64 / total >= 0.85,
            "recall@5 below target in {mode} mode: {misses:?}"
        );
    }
}
