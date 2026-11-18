use serde_json::json;
use solascope_config::Config;
use solascope_core::{ContextRequest, Error, SearchMode, SearchRequest, SymbolRequest};
use solascope_engine::Engine;
use solascope_storage::{Store, WriterLock};
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/lib.rs"),
        "fn greet() { println!(\"hello user\"); }\nfn compute_total() -> i32 { 42 }\n",
    )
    .unwrap();
    root
}

fn query(text: &str, mode: SearchMode) -> SearchRequest {
    SearchRequest {
        query: text.into(),
        mode,
        rerank: false,
        ..Default::default()
    }
}

async fn mock_embedding(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let embeddings: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "embeddings": embeddings }))
        })
        .mount(server)
        .await;
}

fn with_embedding(root: &std::path::Path, server: &MockServer) -> Config {
    let mut config = Config::default_for_root(root).unwrap();
    config.embedding.provider = "ollama".into();
    config.embedding.endpoint = server.uri();
    config.embedding.model = "test-model".into();
    config.embedding.dimensions = 2;
    config.embedding.retries = 0;
    config
}

#[tokio::test]
async fn lexical_pipeline_handles_edits_renames_and_deletions() {
    let root = fixture();
    let engine = Engine::new(Config::default_for_root(root.path()).unwrap()).unwrap();
    let first = engine.index(false).await.unwrap();
    assert_eq!(first.files_updated, 1);
    let response = engine
        .search(query("hello", SearchMode::Lexical))
        .await
        .unwrap();
    assert_eq!(response.results[0].relative_path, "src/lib.rs");
    assert_eq!(response.results[0].symbol.as_deref(), Some("greet"));
    assert_eq!(response.results[0].start_line, 1);
    assert!(!response.results[0].stale);
    assert_eq!(engine.index(false).await.unwrap().files_updated, 0);
    fs::write(
        root.path().join("src/lib.rs"),
        "fn welcome() { println!(\"new marker\"); }\n",
    )
    .unwrap();
    assert!(
        engine
            .search(query("hello", SearchMode::Lexical))
            .await
            .unwrap()
            .results[0]
            .stale
    );
    engine.index(false).await.unwrap();
    assert!(engine
        .search(query("hello", SearchMode::Lexical))
        .await
        .unwrap()
        .results
        .is_empty());
    fs::rename(
        root.path().join("src/lib.rs"),
        root.path().join("src/renamed.rs"),
    )
    .unwrap();
    let report = engine.index(false).await.unwrap();
    assert_eq!(report.files_updated, 1);
    assert_eq!(report.files_deleted, 1);
    let response = engine
        .find_symbol(SymbolRequest {
            name: "welcome".into(),
            kind: None,
            path_filter: Some("src/*.rs".into()),
            limit: 10,
        })
        .unwrap();
    assert_eq!(response.results[0].relative_path, "src/renamed.rs");
    fs::remove_file(root.path().join("src/renamed.rs")).unwrap();
    engine.index(false).await.unwrap();
    assert_eq!(engine.status().unwrap().files, 0);
    assert!(engine
        .search(query("welcome", SearchMode::Lexical))
        .await
        .unwrap()
        .results
        .is_empty());
}

#[tokio::test]
async fn changed_function_reuses_unchanged_embedding_and_query_cache() {
    let root = fixture();
    let server = MockServer::start().await;
    mock_embedding(&server).await;
    let engine = Engine::new(with_embedding(root.path(), &server)).unwrap();
    let first = engine.index(false).await.unwrap();
    assert_eq!(first.embedded_chunks, 2);
    let request_count = server.received_requests().await.unwrap().len();
    assert_eq!(engine.index(false).await.unwrap().embedded_chunks, 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        request_count
    );
    fs::write(
        root.path().join("src/lib.rs"),
        "fn greet() { println!(\"hello modified user\"); }\nfn compute_total() -> i32 { 42 }\n",
    )
    .unwrap();
    let changed = engine.index(false).await.unwrap();
    assert_eq!(changed.embedded_chunks, 1);
    assert_eq!(changed.cache_hits, 1);
    engine
        .search(query("greet", SearchMode::Hybrid))
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap().len();
    engine
        .search(query("greet", SearchMode::Hybrid))
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn fingerprint_changes_require_rebuild_and_cache_gc_keeps_live_vectors() {
    let root = fixture();
    let server = MockServer::start().await;
    mock_embedding(&server).await;
    let config = with_embedding(root.path(), &server);
    Engine::new(config.clone())
        .unwrap()
        .index(false)
        .await
        .unwrap();
    let mut changed = config;
    changed.embedding.model = "different-model".into();
    let engine = Engine::new(changed).unwrap();
    assert!(matches!(
        engine.index(false).await,
        Err(Error::ModelIncompatible(_))
    ));
    assert!(matches!(
        engine.search(query("greet", SearchMode::Semantic)).await,
        Err(Error::ModelIncompatible(_))
    ));
    assert!(engine.index(true).await.unwrap().rebuilt);
    assert_eq!(
        engine
            .status()
            .unwrap()
            .embedding_fingerprint
            .unwrap()
            .model,
        "different-model"
    );
    let before = engine.status().unwrap().vectors;
    assert_eq!(engine.prune_cache().unwrap(), 2);
    assert_eq!(engine.status().unwrap().vectors, before);
    assert!(!engine
        .search(query("greet", SearchMode::Semantic))
        .await
        .unwrap()
        .results
        .is_empty());
}

#[tokio::test]
async fn hybrid_fails_open_but_semantic_mode_reports_model_failure() {
    let root = fixture();
    let server = MockServer::start().await;
    mock_embedding(&server).await;
    let engine = Engine::new(with_embedding(root.path(), &server)).unwrap();
    engine.index(false).await.unwrap();
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let response = engine
        .search(query("greet", SearchMode::Hybrid))
        .await
        .unwrap();
    assert!(!response.results.is_empty());
    assert!(!response.warnings.is_empty());
    assert!(response
        .results
        .iter()
        .all(|result| !result.match_sources.iter().any(|source| source == "vector")));
    assert!(matches!(
        engine
            .search(query("semantic unavailable", SearchMode::Semantic))
            .await,
        Err(Error::ModelUnavailable(_))
    ));
}

#[tokio::test]
async fn rerank_failure_is_configurably_fail_open() {
    let root = fixture();
    let mut config = Config::default_for_root(root.path()).unwrap();
    config.privacy.remote_opt_in = true;
    config.rerank.provider = "remote".into();
    config.rerank.model = "test".into();
    config.rerank.endpoint = "http://127.0.0.1:1".into();
    config.rerank.api_key_env = "SOLASCOPE_TEST_UNSET_RERANK_KEY".into();
    config.rerank.timeout_secs = 1;
    let engine = Engine::new(config.clone()).unwrap();
    engine.index(false).await.unwrap();
    let request = SearchRequest {
        rerank: true,
        ..query("greet", SearchMode::Lexical)
    };
    let response = engine.search(request.clone()).await.unwrap();
    assert!(!response.results.is_empty());
    assert!(response
        .warnings
        .iter()
        .any(|warning| warning.contains("reranker unavailable")));
    config.rerank.fail_open = false;
    assert!(Engine::new(config).unwrap().search(request).await.is_err());
}

#[tokio::test]
async fn current_privacy_rules_apply_to_old_index_and_context() {
    let root = fixture();
    fs::write(root.path().join(".env"), "SECRET=do-not-index\n").unwrap();
    let config = Config::default_for_root(root.path()).unwrap();
    let engine = Engine::new(config.clone()).unwrap();
    engine.index(false).await.unwrap();
    assert!(engine
        .search(query("do-not-index", SearchMode::Lexical))
        .await
        .unwrap()
        .results
        .is_empty());
    assert!(engine
        .context(ContextRequest {
            path: ".env".into(),
            ..Default::default()
        })
        .is_err());
    assert!(engine
        .context(ContextRequest {
            path: "../outside".into(),
            ..Default::default()
        })
        .is_err());
    let context = engine
        .context(ContextRequest {
            path: "src/lib.rs".into(),
            start_line: Some(1),
            end_line: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(context.lines.len(), 2);
    assert_eq!(context.lines[1].line, 2);
    let mut restricted = config;
    restricted.project.exclude.push("src/**".into());
    assert!(Engine::new(restricted)
        .unwrap()
        .search(query("greet", SearchMode::Lexical))
        .await
        .unwrap()
        .results
        .is_empty());
}

#[tokio::test]
async fn changed_revision_during_model_request_does_not_publish_partial_snapshot() {
    let root = fixture();
    let server = MockServer::start().await;
    let mutate = Arc::new(AtomicBool::new(false));
    let file = root.path().join("src/lib.rs");
    let flag = mutate.clone();
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(move |request: &Request| {
            if flag.load(Ordering::SeqCst) {
                fs::write(&file, "fn replaced_during_request() {}\n").unwrap();
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let vectors: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "embeddings": vectors }))
        })
        .mount(&server)
        .await;
    let engine = Engine::new(with_embedding(root.path(), &server)).unwrap();
    engine.index(false).await.unwrap();
    let version = engine.status().unwrap().index_version;
    fs::write(root.path().join("src/lib.rs"), "fn proposed_update() {}\n").unwrap();
    mutate.store(true, Ordering::SeqCst);
    assert!(matches!(
        engine.index(false).await,
        Err(Error::SourceChanged(_))
    ));
    assert_eq!(engine.status().unwrap().index_version, version);
    assert_eq!(engine.status().unwrap().pending_jobs, 0);
    assert!(!engine
        .find_symbol(SymbolRequest {
            name: "greet".into(),
            kind: None,
            path_filter: None,
            limit: 10
        })
        .unwrap()
        .results
        .is_empty());
}

#[tokio::test]
async fn abandoned_jobs_are_recovered_without_losing_the_snapshot() {
    let root = fixture();
    let config = Config::default_for_root(root.path()).unwrap();
    let engine = Engine::new(config.clone()).unwrap();
    engine.index(false).await.unwrap();
    {
        let lock = WriterLock::acquire(&config.project.index_dir).unwrap();
        let store = Store::open(&config.project.index_dir, root.path(), true).unwrap();
        store.begin_job(&lock).unwrap();
    }
    assert_eq!(engine.status().unwrap().pending_jobs, 1);
    let report = engine.index(false).await.unwrap();
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("recovered")));
    assert_eq!(engine.status().unwrap().pending_jobs, 0);
    assert!(!engine
        .search(query("greet", SearchMode::Lexical))
        .await
        .unwrap()
        .results
        .is_empty());
}

#[tokio::test]
async fn cancellation_does_not_publish_an_index() {
    let root = fixture();
    let engine = Engine::new(Config::default_for_root(root.path()).unwrap()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        engine.index_with_cancel(false, cancel).await,
        Err(Error::Cancelled)
    ));
    assert!(matches!(engine.status(), Err(Error::IndexMissing)));
}

#[tokio::test]
async fn explicit_rebuild_advances_an_empty_index_generation() {
    let root = tempfile::tempdir().unwrap();
    let engine = Engine::new(Config::default_for_root(root.path()).unwrap()).unwrap();
    let first = engine.index(false).await.unwrap();
    assert_eq!(first.files_total, 0);
    let rebuilt = engine.index(true).await.unwrap();
    assert!(rebuilt.rebuilt);
    assert_eq!(rebuilt.index_version, first.index_version + 1);
}

#[tokio::test]
async fn initial_index_splits_inputs_larger_than_provider_request_limit() {
    let root = fixture();
    let count = solascope_models::MAX_EMBEDDING_INPUTS + 1;
    let source: String = (0..count)
        .map(|number| format!("fn generated_{number}() -> usize {{ {number} }}\n"))
        .collect();
    fs::write(root.path().join("src/lib.rs"), source).unwrap();
    let server = MockServer::start().await;
    mock_embedding(&server).await;
    let mut config = with_embedding(root.path(), &server);
    config.embedding.batch_size = 256;
    config.embedding.concurrency = 32;
    let engine = Engine::new(config).unwrap();
    let report = engine.index(false).await.unwrap();
    assert_eq!(report.embedded_chunks, count);
    for request in server.received_requests().await.unwrap() {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body["input"].as_array().unwrap().len() <= 128);
        assert!(!request.headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn watcher_debounces_changes_and_holds_the_single_writer_lock() {
    let root = fixture();
    let mut config = Config::default_for_root(root.path()).unwrap();
    config.index.debounce_ms = 25;
    let engine = Engine::new(config).unwrap();
    let cancel = CancellationToken::new();
    let task_engine = engine.clone();
    let token = cancel.clone();
    let task = tokio::spawn(async move { task_engine.watch(token).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.status().is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(engine.index(false).await, Err(Error::WriterBusy)));
    fs::write(root.path().join("src/lib.rs"), "fn updated_by_watch() {}\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine
                .find_symbol(SymbolRequest {
                    name: "updated_by_watch".into(),
                    kind: None,
                    path_filter: None,
                    limit: 10,
                })
                .is_ok_and(|response| !response.results.is_empty() && !response.results[0].stale)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(engine.index(false).await.is_ok());
}

#[tokio::test]
async fn watcher_retries_a_source_change_during_its_initial_index() {
    let root = fixture();
    let server = MockServer::start().await;
    let first = Arc::new(AtomicBool::new(true));
    let source = root.path().join("src/lib.rs");
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .respond_with(move |request: &Request| {
            if first.swap(false, Ordering::SeqCst) {
                fs::write(&source, "fn saved_during_startup() {}\n").unwrap();
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let vectors: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|_| vec![1.0, 0.25])
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": vectors}))
        })
        .mount(&server)
        .await;
    let mut config = with_embedding(root.path(), &server);
    config.index.debounce_ms = 25;
    let engine = Engine::new(config).unwrap();
    let cancel = CancellationToken::new();
    let watched = engine.clone();
    let token = cancel.clone();
    let task = tokio::spawn(async move { watched.watch(token).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.status().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    let result = engine
        .find_symbol(SymbolRequest {
            name: "saved_during_startup".into(),
            kind: None,
            path_filter: None,
            limit: 10,
        })
        .unwrap();
    assert!(!result.results.is_empty());
    assert!(!result.results[0].stale);
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
