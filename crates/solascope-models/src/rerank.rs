use std::collections::HashSet;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use solascope_config::{PrivacyConfig, RerankConfig};
use solascope_core::{Error, RerankDocument, RerankLimits, RerankProvider, RerankScore, Result};
use tokio::sync::Semaphore;

use crate::{
    http::{endpoint, HttpTransport},
    MAX_RERANK_DOCUMENTS, MAX_TOTAL_INPUT_CHARS,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Compatible,
    DashScope,
}

const DASHSCOPE_PATH: &str = "/api/v1/services/rerank/text-rerank/text-rerank";

pub(crate) struct HttpReranker {
    transport: HttpTransport,
    provider: String,
    protocol: Protocol,
    model: String,
    top_n: usize,
    max_chars: usize,
    permits: Semaphore,
}

impl HttpReranker {
    pub(crate) fn new(config: &RerankConfig, privacy: &PrivacyConfig) -> Result<Self> {
        validate_config(config)?;
        if !privacy.remote_opt_in {
            return Err(Error::Privacy(
                "remote rerank requires privacy.remote_opt_in=true".into(),
            ));
        }
        let (protocol, suffix) = match config.provider.as_str() {
            "remote" => (Protocol::Compatible, "/rerank"),
            "openai"
                if config
                    .endpoint
                    .trim_end_matches('/')
                    .ends_with("/compatible-api/v1") =>
            {
                (Protocol::Compatible, "/reranks")
            }
            "openai" | "siliconflow" => (Protocol::Compatible, "/v1/rerank"),
            "dashscope" => (Protocol::DashScope, DASHSCOPE_PATH),
            _ => return Err(Error::Config("unsupported rerank provider".into())),
        };
        let (url, remote) = endpoint(&config.endpoint, suffix)?;
        Ok(Self {
            transport: HttpTransport::new(
                url,
                config.api_key_env.clone(),
                config.timeout_secs,
                2,
                remote,
            )?,
            provider: config.provider.clone(),
            protocol,
            model: config.model.clone(),
            top_n: config.top_n,
            max_chars: config.max_input_chars,
            permits: Semaphore::new(4),
        })
    }
}

#[async_trait]
impl RerankProvider for HttpReranker {
    fn name(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
    fn is_remote(&self) -> bool {
        true
    }
    fn limits(&self) -> RerankLimits {
        RerankLimits {
            max_documents: if self.protocol == Protocol::DashScope
                && self.model == "qwen3-vl-rerank"
            {
                100
            } else {
                MAX_RERANK_DOCUMENTS
            },
            max_input_chars: self.max_chars,
        }
    }

    async fn rerank(&self, query: &str, documents: &[RerankDocument]) -> Result<Vec<RerankScore>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        validate_inputs(query, documents, self.max_chars)?;
        if documents.len() > self.limits().max_documents {
            return Err(Error::InvalidInput(
                "rerank candidate count exceeds the provider limit".into(),
            ));
        }
        let _permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
        let top_n = self.top_n.min(documents.len());
        let texts: Vec<&str> = documents
            .iter()
            .map(|document| document.text.as_str())
            .collect();
        let body = if self.protocol == Protocol::DashScope {
            json!({"model": self.model, "input": {"query": query, "documents": texts},
                "parameters": {"top_n": top_n, "return_documents": false}})
        } else {
            json!({"model": self.model, "query": query, "documents": texts, "top_n": top_n, "return_documents": false})
        };
        let response: RerankResponse = self.transport.post(&body).await?;
        let results = match (self.protocol, response.results, response.output) {
            (Protocol::Compatible, Some(results), None) => results,
            (Protocol::DashScope, None, Some(output)) => output.results,
            _ => {
                return Err(Error::ModelIncompatible(
                    "rerank response does not match the configured protocol".into(),
                ))
            }
        };
        map_scores(
            results
                .into_iter()
                .map(|result| (result.index, result.relevance_score)),
            documents,
            top_n,
        )
    }
}

pub(crate) fn validate_config(config: &RerankConfig) -> Result<()> {
    crate::validate_timeout(config.timeout_secs)?;
    if config.model.trim().is_empty()
        || !(1..=MAX_RERANK_DOCUMENTS).contains(&config.top_n)
        || !(1..=MAX_TOTAL_INPUT_CHARS).contains(&config.max_input_chars)
    {
        return Err(Error::Config(
            "rerank requires model, top_n 1..=256, and max_input_chars 1..=4194304".into(),
        ));
    }
    Ok(())
}

/// The limit applies to the query and each candidate, plus an aggregate budget.
/// Repeated/empty IDs are rejected because they cannot identify stable candidates.
pub(crate) fn validate_inputs(
    query: &str,
    documents: &[RerankDocument],
    max_chars: usize,
) -> Result<()> {
    if documents.len() > MAX_RERANK_DOCUMENTS {
        return Err(Error::InvalidInput(
            "rerank has more than 256 candidates; narrow the candidate set".into(),
        ));
    }
    let query_chars = query.chars().count();
    if query_chars == 0 || query_chars > max_chars {
        return Err(Error::InvalidInput(
            "rerank query exceeds character budget or is empty".into(),
        ));
    }
    let mut total = query_chars;
    let mut ids = HashSet::with_capacity(documents.len());
    for document in documents {
        if document.id.is_empty() || !ids.insert(document.id.as_str()) {
            return Err(Error::InvalidInput(
                "rerank document IDs must be nonempty and unique".into(),
            ));
        }
        let chars = document.text.chars().count();
        if chars == 0 || chars > max_chars {
            return Err(Error::InvalidInput(
                "rerank document exceeds character budget or is empty; split oversized chunks"
                    .into(),
            ));
        }
        total += chars;
        if total > MAX_TOTAL_INPUT_CHARS {
            return Err(Error::InvalidInput(
                "rerank exceeds total character budget".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn map_scores(
    scores: impl IntoIterator<Item = (usize, f64)>,
    documents: &[RerankDocument],
    expected_count: usize,
) -> Result<Vec<RerankScore>> {
    let mut seen = HashSet::with_capacity(documents.len());
    let mut mapped = Vec::new();
    for (index, score) in scores {
        let document = documents
            .get(index)
            .ok_or_else(|| Error::ModelIncompatible("rerank index out of range".into()))?;
        if !seen.insert(index) || !score.is_finite() {
            return Err(Error::ModelIncompatible(
                "rerank contains duplicate indices or nonfinite scores".into(),
            ));
        }
        mapped.push(RerankScore {
            document_id: document.id.clone(),
            index,
            score,
        });
    }
    if mapped.len() != expected_count {
        return Err(Error::ModelIncompatible(
            "rerank result count mismatch".into(),
        ));
    }
    mapped.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
    Ok(mapped)
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Option<Vec<RemoteScore>>,
    output: Option<DashScopeOutput>,
}

#[derive(Deserialize)]
struct DashScopeOutput {
    results: Vec<RemoteScore>,
}
#[derive(Deserialize)]
struct RemoteScore {
    index: usize,
    relevance_score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scores_reject_nan_and_preserve_ids() {
        let docs = vec![RerankDocument {
            id: "original".into(),
            text: "code".into(),
        }];
        assert!(map_scores([(0, f64::NAN)], &docs, 1).is_err());
        assert!(map_scores([(0, f64::INFINITY)], &docs, 1).is_err());
        assert_eq!(
            map_scores([(0, -2.0)], &docs, 1).unwrap()[0].document_id,
            "original"
        );
    }
}
