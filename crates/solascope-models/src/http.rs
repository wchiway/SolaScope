use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER},
    Client, StatusCode, Url,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use solascope_config::{EmbeddingConfig, Environment};
use solascope_core::{Error, Result};

use crate::rate_limit::RateLimiter;

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;

pub(crate) struct HttpTransport {
    client: Client,
    endpoint: Url,
    key_env: String,
    environment: Environment,
    retries: usize,
    timeout: Duration,
    limits: Option<Arc<RateLimiter>>,
}

/// Accept only HTTP(S) URLs with credentials kept in environment variables.
/// Query strings are rejected because they commonly carry tokens.
pub(crate) fn endpoint(base: &str, suffix: &str) -> Result<(Url, bool)> {
    let mut url =
        Url::parse(base).map_err(|_| Error::Config("invalid model endpoint URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config("model endpoint must be HTTP(S), without credentials, query, or fragment; use api_key_env".into()));
    }
    let host = url.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if host == "localhost" {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| Error::Config("invalid local endpoint".into()))?;
    }
    let path = url.path().trim_end_matches('/');
    // An explicit rerank URL may be singular (most compatible providers) or
    // plural (Bailian compatible-api). Preserve it instead of appending again.
    let explicit_rerank =
        suffix.ends_with("/rerank") && (path.ends_with("/rerank") || path.ends_with("/reranks"));
    let path = if path.ends_with(suffix) || explicit_rerank {
        path.to_owned()
    } else {
        let base_parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let suffix_parts: Vec<&str> = suffix.split('/').filter(|part| !part.is_empty()).collect();
        let overlap = (0..=base_parts.len().min(suffix_parts.len()))
            .rev()
            .find(|count| base_parts[base_parts.len() - count..] == suffix_parts[..*count])
            .unwrap_or(0);
        format!(
            "/{}",
            base_parts
                .iter()
                .chain(&suffix_parts[overlap..])
                .copied()
                .collect::<Vec<_>>()
                .join("/")
        )
    };
    url.set_path(&path);
    Ok((url, !loopback))
}

struct ServiceFailure {
    error: Error,
    retry: bool,
    rate_limited: bool,
}

// Error payloads can arrive with HTTP 200 on compatible gateways. Classify
// only known codes and never expose response messages, URLs or credentials.
fn service_failure(status: StatusCode, body: Option<&Value>) -> Option<ServiceFailure> {
    let codes: Vec<String> = body
        .into_iter()
        .flat_map(|value| {
            ["/error/code", "/error/type", "/code", "/type"]
                .into_iter()
                .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        })
        .map(|code| {
            code.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .collect();
    let error_payload = body.is_some_and(|value| {
        value.get("error").is_some_and(|error| !error.is_null())
            || value.get("code").is_some_and(|code| {
                !code.is_null()
                    && !matches!(code.as_str(), Some("" | "0" | "200" | "OK" | "Success"))
                    && code.as_i64() != Some(0)
                    && code.as_i64() != Some(200)
            })
    });
    if status.is_success() && !error_payload {
        return None;
    }
    let is_code = |names: &[&str]| codes.iter().any(|code| names.contains(&code.as_str()));
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::PAYMENT_REQUIRED
    ) || is_code(&[
        "invalidapikey",
        "invalidaccesskeyid",
        "accessdenied",
        "forbidden",
        "permissiondenied",
        "insufficientquota",
        "quotaexceeded",
        "quotaexhausted",
        "billinghardlimitreached",
        "billingnotactive",
        "arrearage",
        "prepaidbilloverdue",
        "postpaidbilloverdue",
    ]) {
        return Some(ServiceFailure {
            error: Error::Authentication("model service rejected authentication or quota".into()),
            retry: false,
            rate_limited: false,
        });
    }
    if status == StatusCode::NOT_FOUND
        || is_code(&[
            "modelnotfound",
            "invalidmodel",
            "unsupportedmodel",
            "invaliddimensions",
            "invaliddimension",
        ])
    {
        return Some(ServiceFailure {
            error: Error::ModelIncompatible(
                "model or model API is unavailable/incompatible".into(),
            ),
            retry: false,
            rate_limited: false,
        });
    }
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || is_code(&["ratelimitexceeded", "ratelimiterror"])
        || codes.iter().any(|code| code.starts_with("throttling"));
    if rate_limited
        || status.is_server_error()
        || is_code(&["serviceunavailable", "internalerror", "timeout"])
    {
        let message = if status.is_success() {
            "model service temporarily unavailable".into()
        } else {
            format!(
                "model service temporarily unavailable (HTTP {})",
                status.as_u16()
            )
        };
        return Some(ServiceFailure {
            error: Error::ModelUnavailable(message),
            retry: true,
            rate_limited,
        });
    }
    let message = if status.is_success() {
        "model service returned an error; check endpoint and model configuration".into()
    } else {
        format!(
            "model service rejected request (HTTP {}); check endpoint and model configuration",
            status.as_u16()
        )
    };
    Some(ServiceFailure {
        error: Error::Config(message),
        retry: false,
        rate_limited: false,
    })
}

impl HttpTransport {
    pub(crate) fn new(
        endpoint: Url,
        key_env: String,
        timeout_secs: u64,
        retries: usize,
        remote: bool,
        environment: Environment,
    ) -> Result<Self> {
        crate::validate_timeout(timeout_secs)?;
        if retries > 5 {
            return Err(Error::Config("embedding retries must be at most 5".into()));
        }
        let timeout = Duration::from_secs(timeout_secs);
        let mut builder = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        if !remote {
            // A proxy must not forward local source code off-machine.
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|_| Error::Config("could not initialize model HTTP client".into()))?;
        Ok(Self {
            client,
            endpoint,
            key_env,
            environment,
            retries,
            timeout,
            limits: None,
        })
    }

    pub(crate) fn with_embedding_limits(mut self, config: &EmbeddingConfig) -> Result<Self> {
        self.limits = Some(RateLimiter::new(config)?);
        Ok(self)
    }

    pub(crate) fn token_limit(&self) -> Option<u64> {
        self.limits.as_ref().and_then(|limits| limits.token_limit())
    }

    pub(crate) async fn post<T: DeserializeOwned>(&self, body: &Value) -> Result<T> {
        self.post_with_tokens(body, 0).await
    }

    /// One deadline covers admission, attempts, streaming and backoff. Dropping
    /// this future cancels HTTP and releases its queued or active request slot.
    pub(crate) async fn post_with_tokens<T: DeserializeOwned>(
        &self,
        body: &Value,
        tokens: u64,
    ) -> Result<T> {
        tokio::time::timeout(self.timeout, self.post_inner(body, tokens))
            .await
            .map_err(|_| Error::ModelUnavailable("model request deadline exceeded".into()))?
    }

    async fn retry_backoff(
        &self,
        attempt: usize,
        total_attempts: usize,
        status: Option<StatusCode>,
        category: &'static str,
        hint: Option<Duration>,
    ) {
        if let Some(limits) = &self.limits {
            let delay = limits.retry_delay(attempt, hint);
            log_failure(
                attempt,
                total_attempts,
                status,
                delay_millis(delay),
                category,
            );
            if let Some(until) = tokio::time::Instant::now().checked_add(delay) {
                tokio::time::sleep_until(until).await;
            } else {
                std::future::pending::<()>().await;
            }
        } else {
            backoff(attempt, total_attempts, status, category).await;
        }
    }

    async fn post_inner<T: DeserializeOwned>(&self, body: &Value, tokens: u64) -> Result<T> {
        let total_attempts = self.retries + 1;
        for attempt in 0..=self.retries {
            let mut request = self.client.post(self.endpoint.clone()).json(body);
            // Resolve live process values or this configuration's file defaults;
            // never expose secrets or HTTP response bodies in diagnostics.
            if !self.key_env.is_empty() {
                let key = self.environment.var(&self.key_env).map_err(|_| {
                    Error::Authentication(
                        "API key environment variable is missing or invalid".into(),
                    )
                })?;
                if key.trim().is_empty() {
                    return Err(Error::Authentication(
                        "API key environment variable is empty".into(),
                    ));
                }
                let mut header = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                    Error::Authentication("API key cannot be used as an HTTP header".into())
                })?;
                header.set_sensitive(true);
                request = request.header(AUTHORIZATION, header);
            }
            let mut permit = match &self.limits {
                Some(limits) => Some(limits.acquire(tokens).await?),
                None => None,
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    if temporary_network(&error) && attempt < self.retries {
                        drop(permit);
                        self.retry_backoff(attempt, total_attempts, None, "network", None)
                            .await;
                        continue;
                    }
                    log_failure(attempt, total_attempts, None, 0, "network");
                    return Err(Error::ModelUnavailable(
                        "model transport failed; check service availability and TLS configuration"
                            .into(),
                    ));
                }
            };
            let status = response.status();
            let hint = retry_after(response.headers());
            let budget = if status.is_success() {
                MAX_RESPONSE_BYTES
            } else {
                MAX_ERROR_BYTES
            };
            let bytes = match read_bounded(response, budget).await {
                Ok(bytes) => bytes,
                // HTTP status alone is sufficient to classify failed responses.
                Err(_) if !status.is_success() => Vec::new(),
                Err(ReadError::Network(error))
                    if temporary_network(&error) && attempt < self.retries =>
                {
                    drop(permit);
                    self.retry_backoff(
                        attempt,
                        total_attempts,
                        Some(status),
                        "response_stream",
                        None,
                    )
                    .await;
                    continue;
                }
                Err(ReadError::Network(_)) => {
                    log_failure(attempt, total_attempts, Some(status), 0, "response_stream");
                    return Err(Error::ModelUnavailable(
                        "model response stream failed".into(),
                    ));
                }
                Err(ReadError::TooLarge) => {
                    log_failure(
                        attempt,
                        total_attempts,
                        Some(status),
                        0,
                        "response_too_large",
                    );
                    return Err(Error::ModelIncompatible(
                        "model response exceeds size budget".into(),
                    ));
                }
            };
            let payload = serde_json::from_slice::<Value>(&bytes).ok();
            if let Some(failure) = service_failure(status, payload.as_ref()) {
                let category = match &failure.error {
                    Error::Authentication(_) => "authentication_or_quota",
                    Error::ModelIncompatible(_) => "model_incompatible",
                    Error::ModelUnavailable(_) if failure.rate_limited => "rate_limited",
                    Error::ModelUnavailable(_) => "model_unavailable",
                    _ => "request_rejected",
                };
                if failure.rate_limited {
                    if let Some(limits) = &self.limits {
                        let delay = match limits.rate_limited(hint) {
                            Ok(delay) => delay,
                            Err(error) => {
                                drop(permit.take());
                                log_failure(
                                    attempt,
                                    total_attempts,
                                    Some(status),
                                    0,
                                    "invalid_retry_after",
                                );
                                return Err(error);
                            }
                        };
                        drop(permit.take());
                        if attempt < self.retries {
                            log_failure(
                                attempt,
                                total_attempts,
                                Some(status),
                                delay_millis(delay),
                                category,
                            );
                            continue;
                        }
                    }
                }
                if failure.retry && attempt < self.retries {
                    drop(permit);
                    self.retry_backoff(attempt, total_attempts, Some(status), category, hint)
                        .await;
                    continue;
                }
                log_failure(attempt, total_attempts, Some(status), 0, category);
                return Err(failure.error);
            }
            let payload = payload.ok_or_else(|| {
                log_failure(attempt, total_attempts, Some(status), 0, "invalid_response");
                Error::ModelIncompatible("model returned invalid or incompatible JSON".into())
            })?;
            let result = serde_json::from_value(payload).map_err(|_| {
                log_failure(attempt, total_attempts, Some(status), 0, "invalid_response");
                Error::ModelIncompatible("model returned invalid or incompatible JSON".into())
            });
            if result.is_ok() {
                if let Some(permit) = permit {
                    permit.succeed();
                }
            }
            return result;
        }
        Err(Error::ModelUnavailable("model retries exhausted".into()))
    }
}

enum ReadError {
    Network(reqwest::Error),
    TooLarge,
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, ReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Network)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ReadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn temporary_network(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_body()
}

fn log_failure(
    attempt: usize,
    total_attempts: usize,
    status: Option<StatusCode>,
    delay_ms: u64,
    category: &'static str,
) {
    tracing::warn!(
        attempt = attempt + 1,
        total_attempts,
        http_status = status.map(|status| status.as_u16()),
        delay_ms,
        retrying = delay_ms > 0,
        error_category = category,
        "model HTTP request failed"
    );
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after(headers.get(RETRY_AFTER)?.to_str().ok()?, SystemTime::now())
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // An overflowing but syntactically valid hint must not become a short
        // fallback retry. The caller's deadline still bounds an enormous wait.
        Some(
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .unwrap_or(Duration::MAX),
        )
    } else {
        httpdate::parse_http_date(value)
            .ok()
            .map(|date| date.duration_since(now).unwrap_or_default())
    }
}

fn delay_millis(delay: Duration) -> u64 {
    delay.as_millis().min(u128::from(u64::MAX)) as u64
}

async fn backoff(
    attempt: usize,
    total_attempts: usize,
    status: Option<StatusCode>,
    category: &'static str,
) {
    let delay_ms = 100 * (1_u64 << attempt.min(5));
    log_failure(attempt, total_attempts, status, delay_ms, category);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use tracing::instrument::WithSubscriber;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn retry_after_parses_seconds_and_http_dates_without_shortening_large_hints() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(
            parse_retry_after(" 12 ", now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(parse_retry_after("0", now), Some(Duration::ZERO));
        let future = httpdate::fmt_http_date(now + Duration::from_secs(60));
        assert_eq!(
            parse_retry_after(&future, now),
            Some(Duration::from_secs(60))
        );
        let past = httpdate::fmt_http_date(now - Duration::from_secs(60));
        assert_eq!(parse_retry_after(&past, now), Some(Duration::ZERO));
        for invalid in ["", "-1", "1.5", "not-a-date"] {
            assert_eq!(parse_retry_after(invalid, now), None);
        }
        assert_eq!(
            parse_retry_after("999999999999999999999999", now),
            Some(Duration::MAX)
        );
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(12)));
        assert_eq!(delay_millis(Duration::MAX), u64::MAX);
    }

    #[tokio::test]
    async fn embedding_cooldown_diagnostics_never_log_headers_sources_or_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .insert_header("X-Vendor-Trace", "header-canary")
                .set_body_json(json!({"error": {"code": "RateLimitExceeded", "message": "vendor-message-canary"}})))
            .expect(3)
            .mount(&server).await;
        let transport = HttpTransport::new(
            Url::parse(&format!("{}/endpoint-canary", server.uri())).unwrap(),
            String::new(),
            3,
            2,
            false,
            Environment::default(),
        )
        .unwrap()
        .with_embedding_limits(&EmbeddingConfig {
            retry_base_delay_ms: 4,
            retry_max_delay_ms: 4,
            rate_limit_cooldown_ms: 4,
            ..Default::default()
        })
        .unwrap();
        let log = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_env_filter("solascope_models=warn")
            .with_writer(Mutex::new(log.reopen().unwrap()))
            .finish();
        let result = transport
            .post::<Value>(&json!({"input": "source-query-canary"}))
            .with_subscriber(subscriber)
            .await;
        assert!(matches!(result, Err(Error::ModelUnavailable(_))));
        let contents = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.contains("model HTTP request failed"))
                .count(),
            3
        );
        assert!(contents.contains("effective_concurrency=1"));
        assert!(contents.contains("delay_ms=4"));
        for sensitive in [
            server.uri().as_str(),
            "endpoint-canary",
            "source-query-canary",
            "vendor-message-canary",
            "header-canary",
        ] {
            assert!(!contents.contains(sensitive), "{contents}");
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn failure_diagnostics_report_retries_without_request_or_response_secrets() {
        for (status, code, delays, category) in [
            (429, "rate_limit_error", vec![100, 200, 0], "rate_limited"),
            (
                503,
                "service_unavailable",
                vec![100, 200, 0],
                "model_unavailable",
            ),
            (
                429,
                "insufficient_quota",
                vec![0],
                "authentication_or_quota",
            ),
            (400, "bad_request", vec![0], "request_rejected"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("X-Vendor-Trace", "header-canary")
                        .set_body_json(json!({
                            "error": {"code": code, "message": "vendor-message-canary"}
                        })),
                )
                .expect(delays.len() as u64)
                .mount(&server)
                .await;
            let mut transport = HttpTransport::new(
                Url::parse(&format!(
                    "{}/endpoint-canary?token=url-token-canary",
                    server.uri()
                ))
                .unwrap(),
                String::new(),
                3,
                2,
                false,
                Environment::default(),
            )
            .unwrap();
            let mut authorization = HeaderValue::from_static("Bearer credential-canary");
            authorization.set_sensitive(true);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(AUTHORIZATION, authorization);
            transport.client = Client::builder()
                .no_proxy()
                .default_headers(headers)
                .build()
                .unwrap();

            let log = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_env_filter("solascope_models::http=warn")
                .with_writer(Mutex::new(log.reopen().unwrap()))
                .finish();
            let body = json!({"input": "source-query-canary"});
            let result = transport
                .post::<Value>(&body)
                .with_subscriber(subscriber)
                .await;
            assert!(result.is_err());
            server.verify().await;

            let contents = std::fs::read_to_string(log.path()).unwrap();
            let events: Vec<_> = contents.lines().collect();
            assert_eq!(events.len(), delays.len());
            for (index, (event, delay_ms)) in events.iter().zip(&delays).enumerate() {
                assert!(event.contains(&format!("attempt={}", index + 1)));
                assert!(event.contains("total_attempts=3"));
                assert!(event.contains(&format!("http_status={status}")));
                assert!(event.contains(&format!("delay_ms={delay_ms}")));
                assert!(event.contains(&format!("retrying={}", *delay_ms != 0)));
                assert!(event.contains(&format!("error_category=\"{category}\"")));
            }
            for sensitive in [
                server.uri().as_str(),
                "endpoint-canary",
                "url-token-canary",
                "credential-canary",
                "source-query-canary",
                "vendor-message-canary",
                "header-canary",
            ] {
                assert!(!contents.contains(sensitive));
            }
        }
    }
}
