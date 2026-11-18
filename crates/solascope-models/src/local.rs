//! Real FastEmbed cross-encoder inference. Offline mode loads a flat bundle
//! containing model.onnx, tokenizer.json, config.json, special_tokens_map.json,
//! and tokenizer_config.json directly; it never invokes a hub client.
//! The dynamic ONNX runtime must be installed separately (ORT_DYLIB_PATH).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use fastembed::{
    RerankInitOptions, RerankInitOptionsUserDefined, RerankerModel, TextRerank, TokenizerFiles,
    UserDefinedRerankingModel,
};
use solascope_config::RerankConfig;
use solascope_core::{Error, RerankDocument, RerankProvider, RerankScore, Result};
use tokio::sync::Semaphore;

use crate::rerank::{map_scores, validate_config, validate_inputs};

const LOCAL_FILES: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const MAX_TOKENS: usize = 512;

#[derive(Clone)]
enum LoadPlan {
    Offline(PathBuf),
    Download {
        model: RerankerModel,
        cache_dir: PathBuf,
    },
}

pub(crate) struct LocalReranker {
    model: String,
    plan: LoadPlan,
    state: Arc<Mutex<Option<TextRerank>>>,
    permit: Arc<Semaphore>,
    top_n: usize,
    max_chars: usize,
    timeout: Duration,
}

impl LocalReranker {
    pub(crate) fn new(config: &RerankConfig) -> Result<Self> {
        validate_config(config)?;
        let cache_dir = config.cache_dir.clone().ok_or_else(|| Error::Config(
            "fastembed requires rerank.cache_dir pointing to a local model bundle or an explicit download cache".into()))?;
        let plan = if LOCAL_FILES
            .iter()
            .all(|name| cache_dir.join(name).is_file())
        {
            LoadPlan::Offline(cache_dir)
        } else if config.allow_model_download {
            let info = TextRerank::list_supported_models().into_iter()
                .find(|info| info.model_code == config.model || info.model.to_string() == config.model)
                .ok_or_else(|| Error::Config("unknown FastEmbed reranker model; select a supported model ID or provide a local ONNX bundle".into()))?;
            LoadPlan::Download {
                model: info.model,
                cache_dir,
            }
        } else {
            return Err(Error::ModelUnavailable(
                "local reranker bundle is missing: populate rerank.cache_dir with model.onnx, tokenizer.json, config.json, special_tokens_map.json and tokenizer_config.json; downloads require allow_model_download=true".into()));
        };
        Ok(Self {
            model: config.model.clone(),
            plan,
            state: Arc::new(Mutex::new(None)),
            permit: Arc::new(Semaphore::new(1)),
            top_n: config.top_n,
            max_chars: config.max_input_chars,
            timeout: Duration::from_secs(config.timeout_secs),
        })
    }
}

fn load(plan: &LoadPlan) -> Result<TextRerank> {
    let result = match plan {
        LoadPlan::Offline(path) => {
            let read = |name: &str| {
                std::fs::read(path.join(name)).map_err(|_| Error::ModelUnavailable("local reranker bundle could not be read; check model files and permissions".into()))
            };
            let files = TokenizerFiles {
                tokenizer_file: read("tokenizer.json")?,
                config_file: read("config.json")?,
                special_tokens_map_file: read("special_tokens_map.json")?,
                tokenizer_config_file: read("tokenizer_config.json")?,
            };
            TextRerank::try_new_from_user_defined(
                UserDefinedRerankingModel::new(path.join("model.onnx"), files),
                RerankInitOptionsUserDefined::new()
                    .with_max_length(MAX_TOKENS)
                    .with_intra_threads(2),
            )
        }
        LoadPlan::Download { model, cache_dir } => TextRerank::try_new(
            RerankInitOptions::new(model.clone())
                .with_cache_dir(cache_dir.clone())
                .with_max_length(MAX_TOKENS)
                .with_intra_threads(2)
                .with_show_download_progress(false),
        ),
    };
    result.map_err(|_| Error::ModelUnavailable("FastEmbed initialization failed; verify the reranker bundle, model compatibility and ONNX runtime configured by ORT_DYLIB_PATH".into()))
}

#[async_trait]
impl RerankProvider for LocalReranker {
    fn name(&self) -> String {
        format!("fastembed/{}", self.model)
    }
    fn is_remote(&self) -> bool {
        false
    }

    async fn rerank(&self, query: &str, documents: &[RerankDocument]) -> Result<Vec<RerankScore>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        validate_inputs(query, documents, self.max_chars)?;
        let work = async {
            // Acquire before spawning: canceled waiters never queue blocking
            // jobs. The running job owns the permit even after an async timeout.
            let permit = self
                .permit
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Cancelled)?;
            let state = self.state.clone();
            let plan = self.plan.clone();
            let query = query.to_owned();
            let documents = documents.to_vec();
            let top_n = self.top_n.min(documents.len());
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut guard = state.lock().map_err(|_| Error::ModelUnavailable("local reranker state is poisoned; rebuild the provider".into()))?;
                if guard.is_none() { *guard = Some(load(&plan)?); }
                let model = guard.as_mut().ok_or_else(|| Error::ModelUnavailable("local reranker did not initialize".into()))?;
                // Inspect full pair lengths without truncation before inference.
                // This prevents the tokenizer from silently removing source code.
                let mut tokenizer = model.tokenizer.clone();
                tokenizer.with_truncation(None).map_err(|_| Error::ModelIncompatible("reranker tokenizer configuration is invalid".into()))?;
                for document in &documents {
                    let encoded = tokenizer.encode((query.as_str(), document.text.as_str()), true)
                        .map_err(|_| Error::ModelIncompatible("reranker could not tokenize the input pair".into()))?;
                    if encoded.len() > MAX_TOKENS {
                        return Err(Error::InvalidInput("local reranker query/document pair exceeds 512 tokens; split chunks or reduce the query".into()));
                    }
                }
                let texts: Vec<&str> = documents.iter().map(|document| document.text.as_str()).collect();
                let scores = model.rerank(query.as_str(), texts, false, Some(16))
                    .map_err(|_| Error::ModelIncompatible("FastEmbed inference failed; verify the cross-encoder model and tokenizer".into()))?;
                // Validate every score, including those outside top_n.
                let mut mapped = map_scores(scores.into_iter().map(|score| (score.index, f64::from(score.score))), &documents, documents.len())?;
                mapped.truncate(top_n);
                Ok(mapped)
            }).await.map_err(|_| Error::ModelUnavailable("local reranker worker failed; verify ONNX runtime installation via ORT_DYLIB_PATH".into()))?
        };
        // ONNX execution already running in spawn_blocking cannot be preempted;
        // bounded candidates/tokens and the held permit prevent runaway queuing.
        tokio::time::timeout(self.timeout, work).await
            .map_err(|_| Error::ModelUnavailable("local reranker deadline exceeded; an active ONNX call may finish in the background".into()))?
    }
}
