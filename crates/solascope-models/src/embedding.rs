use async_trait::async_trait;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::json;
use solascope_config::{EmbeddingConfig, PrivacyConfig};
use solascope_core::{EmbeddingProvider, Error, ModelFingerprint, Result};
use tokio::sync::Semaphore;

use crate::{
    http::{endpoint, HttpTransport},
    normalize_vector, MAX_EMBEDDING_CHARS, MAX_EMBEDDING_INPUTS, MAX_TOTAL_INPUT_CHARS,
};

pub(crate) struct HttpEmbedding {
    transport: HttpTransport,
    fingerprint: ModelFingerprint,
    remote: bool,
    ollama: bool,
    batch_size: usize,
    concurrency: usize,
    permits: Semaphore,
}

impl HttpEmbedding {
    pub(crate) fn new(config: &EmbeddingConfig, privacy: &PrivacyConfig) -> Result<Self> {
        let ollama = match config.provider.as_str() {
            "ollama" => true,
            "openai" => false,
            _ => return Err(Error::Config("unsupported embedding provider".into())),
        };
        if config.model.trim().is_empty()
            || !(1..=65_536).contains(&config.dimensions)
            || !(1..=256).contains(&config.batch_size)
            || !(1..=32).contains(&config.concurrency)
        {
            return Err(Error::Config("embedding requires model, dimensions 1..=65536, batch_size 1..=256, concurrency 1..=32".into()));
        }
        let (url, endpoint_remote) = endpoint(
            &config.endpoint,
            if ollama {
                "/api/embed"
            } else {
                "/v1/embeddings"
            },
        )?;
        let remote = !ollama || endpoint_remote;
        if remote && !privacy.remote_opt_in {
            return Err(Error::Privacy("remote embedding requires privacy.remote_opt_in=true (including non-loopback Ollama)".into()));
        }
        let fingerprint = ModelFingerprint {
            provider: config.provider.clone(),
            model: config.model.clone(),
            endpoint: url.to_string(),
            dimensions: config.dimensions,
            normalization: "l2".into(),
            template_version: "code-v1".into(),
        };
        // The standard Ollama API is unauthenticated; never forward another
        // provider's API key to it just because that environment variable exists.
        let key_env = if ollama {
            String::new()
        } else {
            config.api_key_env.clone()
        };
        let transport = HttpTransport::new(
            url,
            key_env,
            config.timeout_secs,
            config.retries,
            endpoint_remote,
        )?;
        Ok(Self {
            transport,
            fingerprint,
            remote,
            ollama,
            batch_size: config.batch_size,
            concurrency: config.concurrency,
            permits: Semaphore::new(config.concurrency),
        })
    }

    async fn batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let _permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
        let model = &self.fingerprint.model;
        let dimensions = self.fingerprint.dimensions;
        let vectors = if self.ollama {
            // truncate=false prevents Ollama from silently discarding code.
            // No automatic fallback to the obsolete /api/embeddings protocol.
            let response: OllamaResponse = self
                .transport
                .post(&json!({"model": model, "input": texts, "truncate": false}))
                .await?;
            if response.embeddings.len() != texts.len() {
                return Err(Error::ModelIncompatible(
                    "Ollama embedding count mismatch".into(),
                ));
            }
            response.embeddings
        } else {
            let response: OpenAiResponse = self.transport.post(&json!({"model": model, "input": texts, "encoding_format": "float", "dimensions": dimensions})).await?;
            if response.data.len() != texts.len() {
                return Err(Error::ModelIncompatible(
                    "OpenAI embedding count mismatch".into(),
                ));
            }
            let mut ordered = vec![None; texts.len()];
            for item in response.data {
                let slot = ordered.get_mut(item.index).ok_or_else(|| {
                    Error::ModelIncompatible("embedding index out of range".into())
                })?;
                if slot.replace(item.embedding).is_some() {
                    return Err(Error::ModelIncompatible("duplicate embedding index".into()));
                }
            }
            ordered
                .into_iter()
                .map(|vector| {
                    vector.ok_or_else(|| Error::ModelIncompatible("missing embedding index".into()))
                })
                .collect::<Result<Vec<_>>>()?
        };
        vectors
            .into_iter()
            .map(|vector| normalize_vector(vector, dimensions))
            .collect()
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedding {
    fn fingerprint(&self) -> ModelFingerprint {
        self.fingerprint.clone()
    }
    fn is_remote(&self) -> bool {
        self.remote
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if texts.len() > MAX_EMBEDDING_INPUTS {
            return Err(Error::InvalidInput(
                "too many embedding inputs; split the request".into(),
            ));
        }
        let mut total = 0usize;
        for text in texts {
            let chars = text.chars().count();
            if chars == 0 || chars > MAX_EMBEDDING_CHARS {
                return Err(Error::InvalidInput(
                    "embedding input must contain 1..=32768 characters; split oversized chunks"
                        .into(),
                ));
            }
            total += chars;
            if total > MAX_TOTAL_INPUT_CHARS {
                return Err(Error::InvalidInput(
                    "embedding request exceeds total character budget".into(),
                ));
            }
        }
        // Futures are owned here (not spawned). Cancellation or a failed batch
        // drops pending requests. buffered preserves batch order even if later
        // batches complete first; individual OpenAI indices restore item order.
        let inputs: Vec<Vec<String>> = texts
            .chunks(self.batch_size)
            .map(<[String]>::to_vec)
            .collect();
        let batches: Vec<Vec<Vec<f32>>> = stream::iter(inputs)
            .map(|batch: Vec<String>| async move { self.batch(&batch).await })
            .buffered(self.concurrency)
            .try_collect()
            .await?;
        Ok(batches.into_iter().flatten().collect())
    }
}

#[derive(Deserialize)]
struct OllamaResponse {
    embeddings: Vec<Vec<f32>>,
}
#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbedding>,
}
#[derive(Deserialize)]
struct OpenAiEmbedding {
    index: usize,
    embedding: Vec<f32>,
}
