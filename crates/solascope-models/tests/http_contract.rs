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
        retry_base_delay_ms: 10,
        retry_max_delay_ms: 100,
        rate_limit_cooldown_ms: 20,
        ..Default::default()
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
        environment: Default::default(),
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
async fn completed_batches_refill_slots_before_a_slow_batch_and_preserve_order() {
    let server = MockServer::start().await;
    let third_started = Arc::new(tokio::sync::Notify::new());
    let notification = third_started.clone();
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |request: &Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let input = body["input"][0].as_str().unwrap();
            let (vector, delay) = match input {
                "slow" => (json!([1.0, 0.0]), Duration::from_secs(2)),
                "fast" => (json!([0.0, 1.0]), Duration::ZERO),
                "next" => {
                    notification.notify_one();
                    (json!([-1.0, 0.0]), Duration::ZERO)
                }
                _ => panic!("unexpected input"),
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"index": 0, "embedding": vector}]}))
                .set_delay(delay)
        })
        .expect(3)
        .mount(&server)
        .await;
    let mut config = embedding(&server, "openai");
    config.batch_size = 1;
    config.concurrency = 2;
    config.timeout_secs = 5;
    let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
    assert_eq!(provider.batch_size_hint(), Some(1));
    let running = tokio::spawn(async move {
        provider
            .embed(&["slow".into(), "fast".into(), "next".into()])
            .await
    });
    let refilled = tokio::time::timeout(Duration::from_secs(1), third_started.notified()).await;
    if refilled.is_err() {
        running.abort();
        panic!("a completed request must refill its slot before the slow request returns");
    }
    let result = running.await.unwrap().unwrap();
    assert_eq!(
        result,
        vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![-1.0, 0.0]]
    );
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
    config.retry_base_delay_ms = 100;
    config.retry_max_delay_ms = 1_000;
    config.rate_limit_cooldown_ms = 200;
    config.requests_per_minute = Some(60);
    config.tokens_per_minute = Some(12_345);
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
        "timeout_max",
        "retries",
        "retry_base",
        "retry_max",
        "cooldown",
        "base_above_max",
        "cooldown_above_max",
        "rpm",
        "tpm",
    ] {
        let mut config = embedding(&server, "ollama");
        match invalid {
            "dimensions" => config.dimensions = 0,
            "batch_size" => config.batch_size = 0,
            "concurrency" => config.concurrency = 0,
            "timeout" => config.timeout_secs = 0,
            "timeout_max" => config.timeout_secs = 301,
            "retries" => config.retries = 6,
            "retry_base" => config.retry_base_delay_ms = 0,
            "retry_max" => config.retry_max_delay_ms = 300_001,
            "cooldown" => config.rate_limit_cooldown_ms = 0,
            "base_above_max" => config.retry_base_delay_ms = config.retry_max_delay_ms + 1,
            "cooldown_above_max" => config.rate_limit_cooldown_ms = config.retry_max_delay_ms + 1,
            "rpm" => config.requests_per_minute = Some(0),
            "tpm" => config.tokens_per_minute = Some(0),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                build_embedding(&config, &privacy(false)),
                Err(Error::Config(_))
            ),
            "{invalid}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn dashscope_batches_follow_model_limits_and_restore_document_order() {
    for (model, limit, legacy) in [
        ("text-embedding-v1", 25, true),
        ("text-embedding-v2", 25, true),
        ("text-embedding-v3", 10, false),
        ("text-embedding-v4", 10, false),
        ("qwen3.7-text-embedding", 20, false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/services/embeddings/text-embedding/text-embedding",
            ))
            .respond_with(move |request: &Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["model"], model);
                assert_eq!(body["parameters"]["text_type"], "document");
                assert!(body["input"].get("dimensions").is_none());
                if legacy {
                    assert!(body["parameters"].get("dimension").is_none());
                    assert!(body["parameters"].get("output_type").is_none());
                } else {
                    assert_eq!(body["parameters"]["dimension"], 2);
                    assert_eq!(body["parameters"]["output_type"], "dense");
                }
                let inputs = body["input"]["texts"].as_array().unwrap();
                assert!(!inputs.is_empty() && inputs.len() <= limit);
                let entries: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(index, text)| {
                        let number: f32 = text.as_str().unwrap().parse().unwrap();
                        if legacy {
                            json!({"text_index": index, "embedding": [number + 1.0, 1.0]})
                        } else {
                            json!({"index": index, "embedding": [number + 1.0, 1.0]})
                        }
                    })
                    .collect();
                let response = if model.starts_with("qwen3.7") {
                    json!({"embeddings": entries})
                } else {
                    json!({"output": {"embeddings": entries}})
                };
                let delay = if inputs[0] == "0" { 50 } else { 0 };
                ResponseTemplate::new(200)
                    .set_body_json(response)
                    .set_delay(Duration::from_millis(delay))
            })
            .expect(2)
            .mount(&server)
            .await;
        let mut config = embedding(&server, "dashscope");
        config.model = model.into();
        config.endpoint = format!("{}/api/v1/", server.uri());
        config.batch_size = 256;
        let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
        assert_eq!(provider.batch_size_hint(), Some(limit));
        let inputs: Vec<_> = (0..limit + 2).map(|number| number.to_string()).collect();
        let vectors = provider.embed(&inputs).await.unwrap();
        assert_eq!(vectors.len(), inputs.len());
        for (index, vector) in vectors.iter().enumerate() {
            let first = index as f32 + 1.0;
            let norm = first.hypot(1.0);
            assert!((vector[0] - first / norm).abs() < 1e-6);
            assert!((vector[1] - 1.0 / norm).abs() < 1e-6);
        }
        assert!(provider.is_remote());
    }
}

#[tokio::test]
async fn dashscope_uses_a_query_embedding_instead_of_a_document_embedding() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/services/embeddings/text-embedding/text-embedding",
        ))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let vector = match body["parameters"]["text_type"].as_str().unwrap() {
                "document" => {
                    assert_eq!(body["input"]["texts"], json!(["code"]));
                    vec![2.0, 0.0]
                }
                "query" => {
                    assert_eq!(body["input"]["texts"], json!(["where is code"]));
                    vec![0.0, 3.0]
                }
                other => panic!("unexpected text_type: {other}"),
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "output": {"embeddings": [{"text_index": 0, "embedding": vector}]}
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "dashscope"), &privacy(true))
        .unwrap()
        .unwrap();
    assert_eq!(
        provider.embed(&["code".into()]).await.unwrap(),
        vec![vec![1.0, 0.0]]
    );
    assert_eq!(
        provider.embed_query("where is code").await.unwrap(),
        vec![0.0, 1.0]
    );
}

#[tokio::test]
async fn dashscope_rejects_ambiguous_missing_and_corrupt_embedding_results() {
    let valid = json!([{ "index": 0, "embedding": [1.0, 0.0] }]);
    for response in [
        json!({"output": {"embeddings": []}}),
        json!({"output": {"embeddings": [{"index": 1, "embedding": [1.0, 0.0]}]}}),
        json!({"output": {"embeddings": [{"embedding": [1.0, 0.0]}]}}),
        json!({"output": {"embeddings": [{"index": 0, "embedding": [0.0, 0.0]}]}}),
        json!({"output": {"embeddings": [{"index": 0, "embedding": [1.0]}]}}),
        json!({"output": {"embeddings": valid}, "embeddings": valid}),
        json!({"output": {}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        let provider = build_embedding(&embedding(&server, "dashscope"), &privacy(true))
            .unwrap()
            .unwrap();
        assert!(matches!(
            provider.embed_query("query").await,
            Err(Error::ModelIncompatible(_))
        ));
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"output": {"embeddings": [
                {"text_index": 0, "embedding": [1.0, 0.0]},
                {"text_index": 0, "embedding": [0.0, 1.0]}
            ]}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "dashscope"), &privacy(true))
        .unwrap()
        .unwrap();
    assert!(matches!(
        provider.embed(&["a".into(), "b".into()]).await,
        Err(Error::ModelIncompatible(_))
    ));
}

#[tokio::test]
async fn compatible_embedding_dimension_requests_follow_the_selected_protocol() {
    for (provider_name, model, request_dimensions, sends_dimensions) in [
        ("openai", "test-model", true, true),
        ("openai", "test-model", false, false),
        ("remote", "test-model", false, false),
        ("siliconflow", "BAAI/bge-m3", true, false),
        ("siliconflow", "Qwen/Qwen3-Embedding-0.6B", true, true),
        ("siliconflow", "Qwen/Qwen3-Embedding-8B", false, false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(move |request: &Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["model"], model);
                assert_eq!(body["input"], json!(["query"]));
                assert_eq!(body["encoding_format"], "float");
                assert_eq!(body.get("dimensions").is_some(), sends_dimensions);
                if sends_dimensions {
                    assert_eq!(body["dimensions"], 2);
                }
                assert!(body.get("parameters").is_none());
                ResponseTemplate::new(200).set_body_json(json!({"data": [
                    {"index": 0, "embedding": [3.0, 4.0]}
                ]}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let mut config = embedding(&server, provider_name);
        config.endpoint = format!("{}/v1/embeddings/", server.uri());
        config.model = model.into();
        config.request_dimensions = request_dimensions;
        let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
        assert_eq!(provider.embed_query("query").await.unwrap(), vec![0.6, 0.8]);
    }
}

#[tokio::test]
async fn fingerprints_track_effective_dimension_parameters_not_unused_flags() {
    let server = MockServer::start().await;
    for (provider_name, model, changes) in [
        ("openai", "test-model", true),
        ("remote", "test-model", true),
        ("dashscope", "text-embedding-v4", true),
        ("dashscope", "text-embedding-v2", false),
        ("siliconflow", "BAAI/bge-m3", false),
        ("ollama", "test-model", false),
    ] {
        let mut config = embedding(&server, provider_name);
        config.model = model.into();
        let first = build_embedding(&config, &privacy(true))
            .unwrap()
            .unwrap()
            .fingerprint();
        config.request_dimensions = false;
        let second = build_embedding(&config, &privacy(true))
            .unwrap()
            .unwrap()
            .fingerprint();
        assert_eq!(first != second, changes, "{provider_name}/{model}");
    }
}

#[tokio::test]
async fn cloud_rerank_protocols_preserve_explicit_paths_and_original_candidate_ids() {
    for (provider_name, base_path, expected_path, native) in [
        ("remote", "", "/rerank", false),
        ("openai", "", "/v1/rerank", false),
        ("openai", "/v1/", "/v1/rerank", false),
        (
            "openai",
            "/compatible-api/v1",
            "/compatible-api/v1/reranks",
            false,
        ),
        ("openai", "/custom/reranks/", "/custom/reranks", false),
        ("siliconflow", "/v1", "/v1/rerank", false),
        (
            "dashscope",
            "/api/v1",
            "/api/v1/services/rerank/text-rerank/text-rerank",
            true,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(expected_path))
            .respond_with(move |request: &Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["model"], "test-reranker");
                if native {
                    assert_eq!(body["input"]["query"], "find beta");
                    assert_eq!(
                        body["input"]["documents"],
                        json!(["alpha code", "beta code"])
                    );
                    assert_eq!(body["parameters"]["top_n"], 2);
                    assert_eq!(body["parameters"]["return_documents"], false);
                    assert!(body.get("query").is_none());
                } else {
                    assert_eq!(body["query"], "find beta");
                    assert_eq!(body["documents"], json!(["alpha code", "beta code"]));
                    assert_eq!(body["top_n"], 2);
                    assert_eq!(body["return_documents"], false);
                    assert!(body.get("input").is_none());
                }
                let results = json!([
                    {"index": 1, "relevance_score": 0.9},
                    {"index": 0, "relevance_score": 0.1}
                ]);
                let response = if native {
                    json!({"output": {"results": results}})
                } else {
                    json!({"results": results})
                };
                ResponseTemplate::new(200).set_body_json(response)
            })
            .expect(1)
            .mount(&server)
            .await;
        let mut config = rerank(&server);
        config.provider = provider_name.into();
        config.endpoint = format!("{}{base_path}", server.uri());
        let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
        let scores = provider.rerank("find beta", &documents()).await.unwrap();
        assert_eq!(scores[0].document_id, "source:beta");
        assert_eq!(scores[0].index, 1);
        assert_eq!(scores[0].score, 0.9);
        assert_eq!(scores[1].document_id, "source:alpha");
        assert_eq!(scores[1].index, 0);
        assert_eq!(provider.name(), format!("{provider_name}/test-reranker"));
        assert!(provider.is_remote());
    }
}

#[tokio::test]
async fn cloud_rerank_rejects_a_response_from_another_protocol() {
    let results =
        json!([{"index": 0, "relevance_score": 0.2}, {"index": 1, "relevance_score": 0.8}]);
    for (provider_name, response) in [
        ("dashscope", json!({"results": results})),
        ("openai", json!({"output": {"results": results}})),
        (
            "siliconflow",
            json!({"results": results, "output": {"results": results}}),
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = rerank(&server);
        config.provider = provider_name.into();
        let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
        assert!(matches!(
            provider.rerank("query", &documents()).await,
            Err(Error::ModelIncompatible(_))
        ));
    }
}

#[tokio::test]
async fn every_cloud_protocol_requires_opt_in_even_for_a_loopback_endpoint() {
    let server = MockServer::start().await;
    for provider_name in ["openai", "remote", "dashscope", "siliconflow"] {
        assert!(matches!(
            build_embedding(&embedding(&server, provider_name), &privacy(false)),
            Err(Error::Privacy(_))
        ));
        let mut config = rerank(&server);
        config.provider = provider_name.into();
        assert!(matches!(
            build_reranker(&config, &privacy(false)),
            Err(Error::Privacy(_))
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn dashscope_text_rerank_enforces_its_declared_model_limit_before_network() {
    let server = MockServer::start().await;
    let mut config = rerank(&server);
    config.provider = "dashscope".into();
    config.model = "qwen3-vl-rerank".into();
    let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
    assert_eq!(provider.limits().max_documents, 100);
    let documents: Vec<_> = (0..101)
        .map(|index| RerankDocument {
            id: format!("id:{index}"),
            text: "small document".into(),
        })
        .collect();
    assert!(matches!(
        provider.rerank("query", &documents).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cloud_error_envelopes_are_classified_without_exposing_provider_messages() {
    let key_name = "SOLASCOPE_MODELS_TEST_CLOUD_AUTH_KEY";
    let secret = "cloud-fixture-secret-not-for-diagnostics";
    std::env::set_var(key_name, secret);
    for provider_name in ["openai", "remote", "dashscope", "siliconflow"] {
        for (status, code, kind) in [
            (200, "InvalidApiKey", 0),
            (400, "Arrearage", 0),
            (429, "insufficient_quota", 0),
            (200, "ModelNotFound", 1),
            (400, "InvalidParameter", 2),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(move |request: &Request| {
                    assert_eq!(
                        request.headers["authorization"].to_str().unwrap(),
                        format!("Bearer {secret}")
                    );
                    ResponseTemplate::new(status)
                        .set_body_json(json!({"code": code, "message": secret}))
                })
                .expect(1)
                .mount(&server)
                .await;
            let mut config = embedding(&server, provider_name);
            config.api_key_env = key_name.into();
            let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
            let error = provider.embed_query("code").await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains(secret));
            assert!(!format!("{error:?}").contains(&server.uri()));
            match kind {
                0 => assert!(matches!(error, Error::Authentication(_))),
                1 => assert!(matches!(error, Error::ModelIncompatible(_))),
                _ => assert!(matches!(error, Error::Config(_))),
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"error": {"code": "InvalidApiKey", "message": secret}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut config = rerank(&server);
        config.provider = provider_name.into();
        config.api_key_env = key_name.into();
        let provider = build_reranker(&config, &privacy(true)).unwrap().unwrap();
        let error = provider.rerank("query", &documents()).await.unwrap_err();
        assert!(matches!(error, Error::Authentication(_)));
        assert!(!format!("{error:?} {error}").contains(secret));
    }
}

#[tokio::test]
async fn transient_success_status_error_envelopes_retry_with_a_bound() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "Throttling.RateQuota", "message": "sensitive echoed request"
        })))
        .expect(3)
        .mount(&server)
        .await;
    let provider = build_embedding(&embedding(&server, "dashscope"), &privacy(true))
        .unwrap()
        .unwrap();
    let error = provider.embed_query("query").await.unwrap_err();
    assert!(matches!(error, Error::ModelUnavailable(_)));
    assert!(!error.to_string().contains("sensitive echoed request"));
}

#[tokio::test]
async fn legacy_dashscope_embedding_rejects_oversized_text_without_truncation() {
    let server = MockServer::start().await;
    let mut config = embedding(&server, "dashscope");
    config.model = "text-embedding-v2".into();
    let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
    let error = provider.embed_query(&"字".repeat(2049)).await.unwrap_err();
    assert!(matches!(error, Error::InvalidInput(_)));
    assert!(error.to_string().contains("2048"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rate_limits_cool_down_query_and_document_requests_even_without_retries() {
    for status in [429, 200] {
        let server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(status).set_body_json(json!({"error": {
                        "code": "Throttling.RateQuota", "message": "Too many requests"
                    }}))
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
                }
            })
            .expect(3)
            .mount(&server)
            .await;
        let config = EmbeddingConfig {
            retries: 0,
            rate_limit_cooldown_ms: 100,
            retry_max_delay_ms: 200,
            ..embedding(&server, "openai")
        };
        let provider = build_embedding(&config, &privacy(true)).unwrap().unwrap();
        assert!(matches!(
            provider.embed(&["limited".into()]).await,
            Err(Error::ModelUnavailable(_))
        ));
        let query_provider = provider.clone();
        let documents = vec!["document".into()];
        let mut requests = Box::pin(async {
            tokio::join!(
                query_provider.embed_query("query"),
                provider.embed(&documents)
            )
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requests.as_mut())
                .await
                .is_err()
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let (query, documents) = requests.await;
        assert_eq!(query.unwrap(), vec![1.0, 0.0]);
        assert_eq!(documents.unwrap(), vec![vec![1.0, 0.0]]);
        server.verify().await;
    }
}

#[tokio::test]
async fn retry_after_is_a_minimum_wait_even_above_the_configured_backoff_cap() {
    for status in [429, 503] {
        let server = MockServer::start().await;
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = times.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                let mut times = captured.lock().unwrap();
                times.push(std::time::Instant::now());
                if times.len() == 1 {
                    ResponseTemplate::new(status).insert_header("Retry-After", "1")
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
        let times = times.lock().unwrap();
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(950));
    }
}

#[tokio::test]
async fn enormous_retry_after_hints_are_bounded_and_do_not_poison_later_requests() {
    for status in [429, 503] {
        let server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(status)
                        .insert_header("Retry-After", "999999999999999999999999")
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let provider = build_embedding(
            &EmbeddingConfig {
                timeout_secs: 1,
                ..embedding(&server, "openai")
            },
            &privacy(true),
        )
        .unwrap()
        .unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            provider.embed(&["code".into()]).await,
            Err(Error::ModelUnavailable(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(provider.embed_query("query").await.unwrap(), vec![1.0, 0.0]);
        server.verify().await;
    }
}

#[tokio::test]
async fn rpm_pacing_counts_retries_and_queries_against_the_same_budget() {
    let server = MockServer::start().await;
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = times.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let mut times = captured.lock().unwrap();
            times.push(std::time::Instant::now());
            if times.len() == 1 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
            }
        })
        .expect(3)
        .mount(&server)
        .await;
    let provider = build_embedding(
        &EmbeddingConfig {
            requests_per_minute: Some(600),
            ..embedding(&server, "openai")
        },
        &privacy(true),
    )
    .unwrap()
    .unwrap();
    provider.embed(&["code".into()]).await.unwrap();
    provider.embed_query("query").await.unwrap();
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 3);
    assert!(times
        .windows(2)
        .all(|pair| pair[1].duration_since(pair[0]) >= Duration::from_millis(90)));
}

#[tokio::test]
async fn token_admission_is_inside_the_deadline_and_cancellation_releases_the_queue() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = build_embedding(
        &EmbeddingConfig {
            concurrency: 1,
            tokens_per_minute: Some(3),
            timeout_secs: 1,
            ..embedding(&server, "openai")
        },
        &privacy(true),
    )
    .unwrap()
    .unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        provider.embed(&["aa".into()]).await,
        Err(Error::ModelUnavailable(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.embed_query("a").await.unwrap(), vec![1.0, 0.0]);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn oversized_utf8_estimates_fail_before_any_embedding_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let provider = build_embedding(
        &EmbeddingConfig {
            tokens_per_minute: Some(5),
            ..embedding(&server, "openai")
        },
        &privacy(true),
    )
    .unwrap()
    .unwrap();
    let error = provider
        .embed(&["a".into(), "你好".into()])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidInput(_)));
    assert!(!error.to_string().contains("你好"));
    server.verify().await;
}
