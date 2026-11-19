use std::{net::IpAddr, time::Duration};

use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Client, StatusCode, Url,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use solascope_core::{Error, Result};

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;

pub(crate) struct HttpTransport {
    client: Client,
    endpoint: Url,
    key_env: String,
    retries: usize,
    timeout: Duration,
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

// Error payloads can arrive with HTTP 200 on compatible gateways. Classify
// only known codes and never expose response messages, URLs or credentials.
fn service_failure(status: StatusCode, body: Option<&Value>) -> Option<(Error, bool)> {
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
        return Some((
            Error::Authentication("model service rejected authentication or quota".into()),
            false,
        ));
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
        return Some((
            Error::ModelIncompatible("model or model API is unavailable/incompatible".into()),
            false,
        ));
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || is_code(&[
            "ratelimitexceeded",
            "ratelimiterror",
            "serviceunavailable",
            "internalerror",
            "timeout",
        ])
        || codes.iter().any(|code| code.starts_with("throttling"))
    {
        let message = if status.is_success() {
            "model service temporarily unavailable".into()
        } else {
            format!(
                "model service temporarily unavailable (HTTP {})",
                status.as_u16()
            )
        };
        return Some((Error::ModelUnavailable(message), true));
    }
    let message = if status.is_success() {
        "model service returned an error; check endpoint and model configuration".into()
    } else {
        format!(
            "model service rejected request (HTTP {}); check endpoint and model configuration",
            status.as_u16()
        )
    };
    Some((Error::Config(message), false))
}

impl HttpTransport {
    pub(crate) fn new(
        endpoint: Url,
        key_env: String,
        timeout_secs: u64,
        retries: usize,
        remote: bool,
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
            retries,
            timeout,
        })
    }

    /// One deadline covers attempts, response streaming and backoff. All work
    /// stays in the caller's future, so dropping it cancels outstanding HTTP.
    pub(crate) async fn post<T: DeserializeOwned>(&self, body: &Value) -> Result<T> {
        tokio::time::timeout(self.timeout, self.post_inner(body))
            .await
            .map_err(|_| Error::ModelUnavailable("model request deadline exceeded".into()))?
    }

    async fn post_inner<T: DeserializeOwned>(&self, body: &Value) -> Result<T> {
        for attempt in 0..=self.retries {
            let mut request = self.client.post(self.endpoint.clone()).json(body);
            // Resolve the secret for each attempt; never retain it on an adapter
            // or include it (or a reqwest error/response body) in diagnostics.
            if !self.key_env.is_empty() {
                let key = std::env::var(&self.key_env).map_err(|_| {
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
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    if temporary_network(&error) && attempt < self.retries {
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(Error::ModelUnavailable(
                        "model transport failed; check service availability and TLS configuration"
                            .into(),
                    ));
                }
            };
            let status = response.status();
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
                    backoff(attempt).await;
                    continue;
                }
                Err(ReadError::Network(_)) => {
                    return Err(Error::ModelUnavailable(
                        "model response stream failed".into(),
                    ))
                }
                Err(ReadError::TooLarge) => {
                    return Err(Error::ModelIncompatible(
                        "model response exceeds size budget".into(),
                    ))
                }
            };
            let payload = serde_json::from_slice::<Value>(&bytes).ok();
            if let Some((error, retry)) = service_failure(status, payload.as_ref()) {
                if retry && attempt < self.retries {
                    backoff(attempt).await;
                    continue;
                }
                return Err(error);
            }
            let payload = payload.ok_or_else(|| {
                Error::ModelIncompatible("model returned invalid or incompatible JSON".into())
            })?;
            return serde_json::from_value(payload).map_err(|_| {
                Error::ModelIncompatible("model returned invalid or incompatible JSON".into())
            });
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

async fn backoff(attempt: usize) {
    tokio::time::sleep(Duration::from_millis(100 * (1_u64 << attempt.min(5)))).await;
}
