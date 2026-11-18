use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::json;
use solascope_config::{EmbeddingConfig, PrivacyConfig, RerankConfig};
use solascope_core::{Error, RerankDocument};
use solascope_models::{build_embedding, build_reranker, MAX_EMBEDDING_CHARS};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

fn privacy(enabled: bool) -> PrivacyConfig {
    PrivacyConfig {
        remote_opt_in: enabled,
        ..Default::default()
    }
}

fn embedding(server: &MockServer, provider: &str) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: provider.into(),
        model: "test-embedding".into(),
        endpoint: server.uri(),
        dimensions: 2,
        api_key_env: String::new(),
        batch_size: 2,
        concurrency: 2,
        timeout_secs: 3,
        retries: 2,
    }
}

fn rerank(server: &MockServer) -> RerankConfig {
    RerankConfig {
        provider: "remote".into(),
        model: "test-reranker".into(),
        endpoint: server.uri(),
        api_key_env: String::new(),
        top_n: 2,
        timeout_secs: 3,
        fail_open: true,
        max_input_chars: 1024,
        cache_dir: None,
        allow_model_download: false,
    }
}

fn documents() -> Vec<RerankDocument> {
    vec![
        RerankDocument {
            id: "source:alpha".into(),
            text: "alpha code".into(),
        },
        RerankDocument {
            id: "source:beta".into(),
            text: "beta code".into(),
        },
    ]
}

#[tokio::test]
async fn factories_are_lazy_and_disabled_ignores_unused_configuration() {
    let server = MockServer::start().await;
    let mut config = embedding(&server, "disabled");
    config.endpoint = "invalid".into();
    assert!(build_embedding(&config, &privacy(false)).unwrap().is_none());
    config.provider = "ollama".into();
    config.endpoint = server.uri();
    let provider = build_embedding(&config, &privacy(false)).unwrap().unwrap();
    assert!(!provider.is_remote());
    assert!(provider.embed(&[]).await.unwrap().is_empty());
    let mut config = rerank(&server);
    config.provider = "disabled".into();
    config.endpoint = "invalid".into();
    assert!(build_reranker(&config, &privacy(false)).unwrap().is_none());
    config.provider = "remote".into();
    config.endpoint = server.uri();
    let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
    assert!(provider.rerank("", &[]).await.unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn remote_providers_require_explicit_opt_in_including_nonlocal_ollama() {
    let server = MockServer::start().await;
    assert!(matches!(
        build_embedding(&embedding(&server, "openai"), &privacy(false)),
        Err(Error::Privacy(_))
    ));
    assert!(matches!(
        build_reranker(&rerank(&server), &privacy(false)),
        Err(Error::Privacy(_))
    ));
    let mut config = embedding(&server, "ollama");
    for host in [
        "http://192.0.2.1:11434",
        "http://localhost.example:11434",
        "http://0.0.0.0:11434",
    ] {
        config.endpoint = host.into();
        assert!(matches!(
            build_embedding(&config, &privacy(false)),
            Err(Error::Privacy(_))
        ));
    }
    for host in [
        "http://localhost:11434",
        "http://127.0.0.1:11434",
        "http://[::1]:11434",
    ] {
        config.endpoint = host.into();
        assert!(build_embedding(&config, &privacy(false)).is_ok());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn openai_restores_indices_and_batch_order_without_duplicating_v1() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/embeddings"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["dimensions"], 2);
            let first = body["input"][0].as_str().unwrap();
            let (data, delay) = if first == "first" {
                (json!([{"index": 1, "embedding": [0.0, 2.0]}, {"index": 0, "embedding": [3.0, 0.0]}]), 100)
            } else {
                (json!([{"index": 0, "embedding": [3.0, 4.0]}]), 0)
            };
            ResponseTemplate::new(200).set_body_json(json!({"data": data})).set_delay(Duration::from_millis(delay))
        }).expect(2).mount(&server).await;
    let mut config = embedding(&server, "openai");
    config.endpoint = format!("{}/v1/", server.uri());
    let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
    let result = provider
        .embed(&["first".into(), "second".into(), "third".into()])
        .await
        .unwrap();
    assert_eq!(result, vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.6, 0.8]]);
    assert!(provider.fingerprint().endpoint.ends_with("/v1/embeddings"));
    assert_eq!(provider.fingerprint().normalization, "l2");
    assert_eq!(provider.fingerprint().template_version, "code-v1");
}

#[tokio::test]
async fn embedding_rejects_count_index_and_vector_corruption() {
    let invalid = [
        json!({"data": []}),
        json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}),
        json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}, {"index": 0, "embedding": [0.0, 1.0]}]}),
        json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}, {"index": 2, "embedding": [0.0, 1.0]}]}),
        json!({"data": [{"index": 0, "embedding": [0.0, 0.0]}, {"index": 1, "embedding": [0.0, 1.0]}]}),
        json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 1, "embedding": [0.0, 1.0]}]}),
    ];
    for body in invalid {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_embedding(&embedding(&server, "openai"), &privacy(true))
            .unwrap()
            .unwrap();
        assert!(matches!(
            provider.embed(&["a".into(), "b".into()]).await,
            Err(Error::ModelIncompatible(_))
        ));
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"data":[{"index":0,"embedding":[1e999,0]}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "openai"), &privacy(true))
        .unwrap()
        .unwrap();
    assert!(matches!(
        provider.embed(&["a".into()]).await,
        Err(Error::ModelIncompatible(_))
    ));
}

#[tokio::test]
async fn ollama_uses_current_api_and_refuses_silent_truncation_or_count_mismatch() {
    for (body, valid) in [
        (json!({"embeddings": [[3.0, 4.0]]}), true),
        (json!({"embeddings": []}), false),
        (json!({"embedding": [3.0, 4.0]}), false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(move |request: &Request| {
                let input: serde_json::Value = request.body_json().unwrap();
                assert_eq!(input["truncate"], false);
                ResponseTemplate::new(200).set_body_json(body.clone())
            })
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_embedding(&embedding(&server, "ollama"), &privacy(false))
            .unwrap()
            .unwrap();
        let result = provider.embed(&["code".into()]).await;
        if valid {
            assert_eq!(result.unwrap(), vec![vec![0.6, 0.8]]);
        } else {
            assert!(matches!(result, Err(Error::ModelIncompatible(_))));
        }
    }
}

#[tokio::test]
async fn authentication_quota_and_configuration_errors_do_not_retry_or_leak() {
    let key_name = "SOLASCOPE_MODELS_TEST_HTTP_AUTH_KEY";
    let key_value = "test-secret-must-not-appear-in-errors";
    std::env::set_var(key_name, key_value);
    for (status, code) in [
        (401, "invalid_api_key"),
        (403, "forbidden"),
        (429, "insufficient_quota"),
        (400, "bad_request"),
        (404, "model_not_found"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &Request| {
                assert_eq!(
                    request
                        .headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    format!("Bearer {key_value}")
                );
                ResponseTemplate::new(status)
                    .set_body_json(json!({"error": {"code": code, "message": key_value}}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let mut config = embedding(&server, "openai");
        config.api_key_env = key_name.into();
        let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
        assert!(!serde_json::to_string(&provider.fingerprint())
            .unwrap()
            .contains(key_value));
        let error = provider.embed(&["code".into()]).await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains(key_value));
        assert!(!format!("{error:?}").contains(&server.uri()));
        match status {
            400 => assert!(matches!(error, Error::Config(_))),
            404 => assert!(matches!(error, Error::ModelIncompatible(_))),
            _ => assert!(matches!(error, Error::Authentication(_))),
        }
    }
}

#[tokio::test]
async fn transient_http_errors_retry_but_stop_at_configured_bound() {
    for status in [429, 503] {
        let server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(status)
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let provider = build_embedding(&embedding(&server, "openai"), &privacy(true))
            .unwrap()
            .unwrap();
        provider.embed(&["code".into()]).await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "openai"), &privacy(true))
        .unwrap()
        .unwrap();
    assert!(matches!(
        provider.embed(&["code".into()]).await,
        Err(Error::ModelUnavailable(_))
    ));
}

#[tokio::test]
async fn rerank_maps_original_ids_and_sends_standard_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/rerank"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["query"], "find beta");
            assert_eq!(body["documents"], json!(["alpha code", "beta code"]));
            assert_eq!(body["top_n"], 2);
            assert_eq!(body["model"], "test-reranker");
            ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"index": 1, "relevance_score": 0.9}, {"index": 0, "relevance_score": -0.2}
            ]}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let mut config = rerank(&server);
    config.endpoint = format!("{}/v1", server.uri());
    let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
    let result = provider.rerank("find beta", &documents()).await.unwrap();
    assert_eq!(result[0].document_id, "source:beta");
    assert_eq!(result[0].index, 1);
    assert_eq!(result[1].document_id, "source:alpha");
    assert_eq!(result[1].score, -0.2);
}

#[tokio::test]
async fn rerank_rejects_empty_duplicate_out_of_range_and_nonfinite_results() {
    for results in [
        json!([]),
        json!([{"index": 0, "relevance_score": 1.0}]),
        json!([{"index": 0, "relevance_score": 1.0}, {"index": 0, "relevance_score": 2.0}]),
        json!([{"index": 0, "relevance_score": 1.0}, {"index": 2, "relevance_score": 2.0}]),
        json!([{"index": 0, "relevance_score": null}, {"index": 1, "relevance_score": 2.0}]),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": results})))
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_reranker(&rerank(&server), &privacy(true))
            .unwrap()
            .unwrap();
        // fail_open=true still returns typed errors; the engine decides fallback.
        assert!(matches!(
            provider.rerank("query", &documents()).await,
            Err(Error::ModelIncompatible(_))
        ));
    }
}

#[tokio::test]
async fn input_budgets_and_duplicate_ids_fail_before_network() {
    let server = MockServer::start().await;
    let provider = build_embedding(&embedding(&server, "ollama"), &privacy(false))
        .unwrap()
        .unwrap();
    assert!(matches!(
        provider.embed(&["x".repeat(MAX_EMBEDDING_CHARS + 1)]).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        provider.embed(&[String::new()]).await,
        Err(Error::InvalidInput(_))
    ));
    let mut config = rerank(&server);
    config.max_input_chars = 4;
    let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
    assert!(matches!(
        provider.rerank("query", &documents()).await,
        Err(Error::InvalidInput(_))
    ));
    let docs = vec![
        RerankDocument {
            id: "same".into(),
            text: "a".into()
        };
        2
    ];
    assert!(matches!(
        provider.rerank("q", &docs).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirects_cannot_forward_local_ollama_requests() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/api/embed", destination.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "ollama"), &privacy(false))
        .unwrap()
        .unwrap();
    assert!(matches!(
        provider.embed(&["code".into()]).await,
        Err(Error::Config(_))
    ));
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn credentials_in_endpoint_are_rejected_without_echoing_them() {
    let server = MockServer::start().await;
    let mut config = embedding(&server, "openai");
    for endpoint in [
        "http://user:secret@localhost:1234",
        "http://localhost:1234?api_key=secret",
        "http://localhost:1234#secret",
    ] {
        config.endpoint = endpoint.into();
        let error = build_embedding(&config, &privacy(true)).err().unwrap();
        assert!(matches!(error, Error::Config(_)));
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn canceled_embedding_does_not_start_remaining_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"embeddings": [[1.0, 0.0]]}))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let mut config = embedding(&server, "ollama");
    config.batch_size = 1;
    config.concurrency = 1;
    let provider = build_embedding(&config, &privacy(false)).unwrap().unwrap();
    let input = vec!["code".into(); 10];
    assert!(
        tokio::time::timeout(Duration::from_millis(75), provider.embed(&input))
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(server.received_requests().await.unwrap().len() <= 1);
}

#[tokio::test]
async fn http_deadline_is_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"embeddings": [[1.0, 0.0]]}))
                .set_delay(Duration::from_secs(2)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut config = embedding(&server, "ollama");
    config.timeout_secs = 1;
    let provider = build_embedding(&config, &privacy(false)).unwrap().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(1500),
        provider.embed(&["code".into()]),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::ModelUnavailable(_))));
}

#[tokio::test]
async fn local_rerank_requires_feature_or_a_real_offline_bundle() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let mut config = rerank(&server);
    config.provider = "fastembed".into();
    config.cache_dir = Some(directory.path().to_owned());
    let error = build_reranker(&config, &privacy(false)).err().unwrap();
    #[cfg(not(feature = "local-rerank"))]
    assert!(matches!(error, Error::Config(_)));
    #[cfg(feature = "local-rerank")]
    assert!(matches!(error, Error::ModelUnavailable(_)));
    assert!(directory.path().read_dir().unwrap().next().is_none());
}

#[tokio::test]
async fn missing_key_is_checked_only_at_request_time_and_is_sanitized() {
    let server = MockServer::start().await;
    let mut config = embedding(&server, "openai");
    config.api_key_env = "SOLASCOPE_MODELS_TEST_NONEXISTENT_KEY_7F843D".into();
    let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
    assert!(provider.embed(&[]).await.unwrap().is_empty());
    let error = provider.embed(&["code".into()]).await.unwrap_err();
    assert!(matches!(error, Error::Authentication(_)));
    assert!(!error.to_string().contains(&config.api_key_env));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fingerprint_tracks_vector_space_but_not_operational_configuration() {
    let server = MockServer::start().await;
    let mut config = embedding(&server, "openai");
    let original = build_embedding(&config, &privacy(true))
        .unwrap()
        .unwrap()
        .fingerprint();
    config.endpoint = format!("{}/v1/embeddings/", server.uri());
    config.api_key_env = "SOLASCOPE_MODELS_TEST_DIFFERENT_KEY_NAME".into();
    config.batch_size = 3;
    config.concurrency = 1;
    config.retries = 0;
    let operational = build_embedding(&config, &privacy(true))
        .unwrap()
        .unwrap()
        .fingerprint();
    assert_eq!(original, operational);
    config.dimensions = 3;
    assert_ne!(
        original,
        build_embedding(&config, &privacy(true))
            .unwrap()
            .unwrap()
            .fingerprint()
    );
    config.dimensions = 2;
    config.model = "another-model".into();
    assert_ne!(
        original,
        build_embedding(&config, &privacy(true))
            .unwrap()
            .unwrap()
            .fingerprint()
    );
}

#[tokio::test]
async fn invalid_operational_limits_fail_during_construction() {
    let server = MockServer::start().await;
    for invalid in [
        "dimensions",
        "batch_size",
        "concurrency",
        "timeout",
        "retries",
    ] {
        let mut config = embedding(&server, "ollama");
        match invalid {
            "dimensions" => config.dimensions = 0,
            "batch_size" => config.batch_size = 0,
            "concurrency" => config.concurrency = 0,
            "timeout" => config.timeout_secs = 0,
            _ => config.retries = 6,
        }
        assert!(matches!(
            build_embedding(&config, &privacy(false)),
            Err(Error::Config(_))
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
