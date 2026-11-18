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
/// Query strings are rejected: they commonly carry tokens and are not needed
/// by the supported protocols. The normalized URL is safe for fingerprints.
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
        // Avoid DNS resolution of a supposedly local destination.
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| Error::Config("invalid local endpoint".into()))?;
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with(suffix) {
        path.to_owned()
    } else if suffix == "/v1/embeddings" && path.ends_with("/v1") {
        format!("{path}/embeddings")
    } else {
        format!("{path}{suffix}")
    };
    url.set_path(&path);
    Ok((url, !loopback))
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
                    let retry = temporary_network(&error);
                    if retry && attempt < self.retries {
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
            if !status.is_success() {
                // The body is inspected only for known error codes, never echoed.
                let bytes = read_bounded(response, MAX_ERROR_BYTES)
                    .await
                    .unwrap_or_default();
                let error_body = serde_json::from_slice::<Value>(&bytes).ok();
                let codes: Vec<&str> = error_body
                    .as_ref()
                    .map(|value| {
                        ["/error/code", "/error/type", "/code", "/type"]
                            .into_iter()
                            .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                let quota = codes.iter().any(|code| {
                    matches!(
                        *code,
                        "insufficient_quota"
                            | "quota_exceeded"
                            | "billing_hard_limit_reached"
                            | "billing_not_active"
                    )
                });
                if quota
                    || matches!(
                        status,
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::PAYMENT_REQUIRED
                    )
                {
                    return Err(Error::Authentication(
                        "model service rejected authentication or quota".into(),
                    ));
                }
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    if attempt < self.retries {
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(Error::ModelUnavailable(format!(
                        "model service temporarily unavailable (HTTP {})",
                        status.as_u16()
                    )));
                }
                if status == StatusCode::NOT_FOUND
                    || codes.iter().any(|code| {
                        matches!(
                            *code,
                            "model_not_found"
                                | "invalid_model"
                                | "unsupported_model"
                                | "invalid_dimensions"
                        )
                    })
                {
                    return Err(Error::ModelIncompatible(
                        "model or model API is unavailable/incompatible".into(),
                    ));
                }
                return Err(Error::Config(format!("model service rejected request (HTTP {}); check endpoint and model configuration", status.as_u16())));
            }
            let bytes = match read_bounded(response, MAX_RESPONSE_BYTES).await {
                Ok(bytes) => bytes,
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
            return serde_json::from_slice(&bytes).map_err(|_| {
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
