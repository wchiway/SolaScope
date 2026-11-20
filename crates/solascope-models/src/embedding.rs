use async_trait::async_trait;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::json;
use solascope_config::{EmbeddingConfig, PrivacyConfig};
use solascope_core::{
    EmbeddingProvider, Error, ModelFingerprint, Result, EMBEDDING_TEMPLATE_VERSION,
};
use tokio::sync::Semaphore;

use crate::{
    http::{endpoint, HttpTransport},
    normalize_vector, MAX_EMBEDDING_CHARS, MAX_EMBEDDING_INPUTS, MAX_TOTAL_INPUT_CHARS,
};

const DASHSCOPE_PATH: &str = "/api/v1/services/embeddings/text-embedding/text-embedding";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Ollama,
    OpenAi,
    DashScope,
}

pub(crate) struct HttpEmbedding {
    transport: HttpTransport,
    fingerprint: ModelFingerprint,
    remote: bool,
    protocol: Protocol,
    request_dimensions: bool,
    batch_size: usize,
    concurrency: usize,
    permits: Semaphore,
}

impl HttpEmbedding {
    pub(crate) fn new(config: &EmbeddingConfig, privacy: &PrivacyConfig) -> Result<Self> {
        let protocol = match config.provider.as_str() {
            "ollama" => Protocol::Ollama,
            "openai" | "remote" | "siliconflow" => Protocol::OpenAi,
            "dashscope" => Protocol::DashScope,
            _ => return Err(Error::Config("unsupported embedding provider".into())),
        };
        if config.model.trim().is_empty()
            || !(1..=65_536).contains(&config.dimensions)
            || !(1..=256).contains(&config.batch_size)
            || !(1..=32).contains(&config.concurrency)
        {
            return Err(Error::Config("embedding requires model, dimensions 1..=65536, batch_size 1..=256, concurrency 1..=32".into()));
        }
        let suffix = match protocol {
            Protocol::Ollama => "/api/embed",
            Protocol::OpenAi => "/v1/embeddings",
            Protocol::DashScope => DASHSCOPE_PATH,
        };
        let (url, endpoint_remote) = endpoint(&config.endpoint, suffix)?;
        let remote = protocol != Protocol::Ollama || endpoint_remote;
        if remote && !privacy.remote_opt_in {
            return Err(Error::Privacy("remote embedding requires privacy.remote_opt_in=true (including non-loopback Ollama)".into()));
        }
        // SiliconFlow documents variable dimensions only for Qwen3 text
        // embedding models. Fixed-dimension models such as BGE omit the field.
        let request_dimensions = config.request_dimensions
            && match protocol {
                Protocol::Ollama => false,
                Protocol::OpenAi if config.provider == "siliconflow" => {
                    config.model.contains("Qwen3-Embedding")
                }
                Protocol::OpenAi => true,
                Protocol::DashScope => !matches!(
                    config.model.as_str(),
                    "text-embedding-v1" | "text-embedding-v2"
                ),
            };
        let template_version = if protocol == Protocol::Ollama
            || (config.provider != "siliconflow"
                && protocol == Protocol::OpenAi
                && request_dimensions)
        {
            EMBEDDING_TEMPLATE_VERSION.to_owned()
        } else {
            format!(
                "{EMBEDDING_TEMPLATE_VERSION};dimensions={}",
                if request_dimensions {
                    "requested"
                } else {
                    "default"
                }
            )
        };
        let fingerprint = ModelFingerprint {
            provider: config.provider.clone(),
            model: config.model.clone(),
            endpoint: url.to_string(),
            dimensions: config.dimensions,
            normalization: "l2".into(),
            template_version,
        };
        // Never forward an unrelated cloud key to the standard Ollama API.
        let key_env = if protocol == Protocol::Ollama {
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
            config.environment.clone(),
        )?;
        let batch_size = if protocol == Protocol::DashScope {
            config.batch_size.min(dashscope_batch_limit(&config.model))
        } else {
            config.batch_size
        };
        Ok(Self {
            transport,
            fingerprint,
            remote,
            protocol,
            request_dimensions,
            batch_size,
            concurrency: config.concurrency,
            permits: Semaphore::new(config.concurrency),
        })
    }

    async fn batch(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let _permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
        let started = std::time::Instant::now();
        let model = &self.fingerprint.model;
        let dimensions = self.fingerprint.dimensions;
        let vectors = match self.protocol {
            Protocol::Ollama => {
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
            }
            Protocol::OpenAi => {
                let mut body = json!({"model": model, "input": texts, "encoding_format": "float"});
                if self.request_dimensions {
                    body["dimensions"] = json!(dimensions);
                }
                let response: OpenAiResponse = self.transport.post(&body).await?;
                ordered(response.data, texts.len())?
            }
            Protocol::DashScope => {
                let mut body = json!({"model": model, "input": {"texts": texts},
                    "parameters": {"text_type": if query { "query" } else { "document" }}});
                if !matches!(model.as_str(), "text-embedding-v1" | "text-embedding-v2") {
                    body["parameters"]["output_type"] = json!("dense");
                }
                if self.request_dimensions {
                    body["parameters"]["dimension"] = json!(dimensions);
                }
                let response: DashScopeResponse = self.transport.post(&body).await?;
                let embeddings = match (response.output, response.embeddings) {
                    (Some(output), None) => output.embeddings,
                    (None, Some(embeddings)) => embeddings,
                    _ => {
                        return Err(Error::ModelIncompatible(
                            "DashScope response must contain exactly one embeddings container"
                                .into(),
                        ))
                    }
                };
                ordered(embeddings, texts.len())?
            }
        };
        let vectors = vectors
            .into_iter()
            .map(|vector| normalize_vector(vector, dimensions))
            .collect::<Result<Vec<_>>>()?;
        tracing::debug!(
            inputs = texts.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            query,
            "embedding batch complete"
        );
        Ok(vectors)
    }

    async fn run(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if texts.len() > MAX_EMBEDDING_INPUTS {
            return Err(Error::InvalidInput(
                "too many embedding inputs; split the request".into(),
            ));
        }
        let max_chars = if self.protocol == Protocol::DashScope
            && matches!(
                self.fingerprint.model.as_str(),
                "text-embedding-v1" | "text-embedding-v2"
            ) {
            2048
        } else {
            MAX_EMBEDDING_CHARS
        };
        let mut total = 0usize;
        for text in texts {
            let chars = text.chars().count();
            if chars == 0 || chars > max_chars {
                return Err(Error::InvalidInput(format!("embedding input must contain 1..={max_chars} characters; split oversized chunks")));
            }
            total += chars;
            if total > MAX_TOTAL_INPUT_CHARS {
                return Err(Error::InvalidInput(
                    "embedding request exceeds total character budget".into(),
                ));
            }
        }
        // Complete ready batches immediately so slow requests do not hold idle
        // slots. Restore input order before associating vectors with source.
        let inputs: Vec<Vec<String>> = texts
            .chunks(self.batch_size)
            .map(<[String]>::to_vec)
            .collect();
        let mut batches: Vec<(usize, Vec<Vec<f32>>)> = stream::iter(inputs.into_iter().enumerate())
            .map(|(index, batch)| async move {
                self.batch(&batch, query)
                    .await
                    .map(|vectors| (index, vectors))
            })
            .buffer_unordered(self.concurrency)
            .try_collect()
            .await?;
        batches.sort_unstable_by_key(|(index, _)| *index);
        Ok(batches
            .into_iter()
            .flat_map(|(_, vectors)| vectors)
            .collect())
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
    fn batch_size_hint(&self) -> Option<usize> {
        Some(self.batch_size)
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.run(texts, false).await
    }
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut vectors = self.run(&[query.to_owned()], true).await?;
        if vectors.len() != 1 {
            return Err(Error::ModelIncompatible(
                "query embedding count must be one".into(),
            ));
        }
        Ok(vectors.remove(0))
    }
}

fn dashscope_batch_limit(model: &str) -> usize {
    if model.starts_with("qwen3.7-text-embedding") {
        20
    } else if matches!(model, "text-embedding-v1" | "text-embedding-v2") {
        25
    } else {
        10
    }
}

fn ordered(items: Vec<IndexedEmbedding>, expected: usize) -> Result<Vec<Vec<f32>>> {
    if items.len() != expected {
        return Err(Error::ModelIncompatible("embedding count mismatch".into()));
    }
    let mut ordered = vec![None; expected];
    for item in items {
        let slot = ordered
            .get_mut(item.index)
            .ok_or_else(|| Error::ModelIncompatible("embedding index out of range".into()))?;
        if slot.replace(item.embedding).is_some() {
            return Err(Error::ModelIncompatible("duplicate embedding index".into()));
        }
    }
    ordered
        .into_iter()
        .map(|vector| {
            vector.ok_or_else(|| Error::ModelIncompatible("missing embedding index".into()))
        })
        .collect()
}

#[derive(Deserialize)]
struct OllamaResponse {
    embeddings: Vec<Vec<f32>>,
}
#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<IndexedEmbedding>,
}
#[derive(Deserialize)]
struct IndexedEmbedding {
    #[serde(alias = "text_index")]
    index: usize,
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct DashScopeOutput {
    embeddings: Vec<IndexedEmbedding>,
}
#[derive(Deserialize)]
struct DashScopeResponse {
    output: Option<DashScopeOutput>,
    embeddings: Option<Vec<IndexedEmbedding>>,
}
