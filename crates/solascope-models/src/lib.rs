//! Model adapters. Construction never probes a service or downloads a checkpoint.
//! Inputs exceeding the published budgets are rejected rather than truncated.

mod embedding;
mod http;
#[cfg(feature = "local-rerank")]
mod local;
mod rerank;

use std::sync::Arc;

use solascope_config::{EmbeddingConfig, PrivacyConfig, RerankConfig};
use solascope_core::{self as core, Error, Result};

/// Maximum Unicode scalar values in one embedding input.
pub const MAX_EMBEDDING_CHARS: usize = 32_768;
pub const MAX_EMBEDDING_INPUTS: usize = 4_096;
pub const MAX_TOTAL_INPUT_CHARS: usize = 4_194_304;
pub const MAX_RERANK_DOCUMENTS: usize = 256;

/// Builds an embedding adapter without making any network requests.
pub fn build_embedding(
    config: &EmbeddingConfig,
    privacy: &PrivacyConfig,
) -> Result<Option<Arc<dyn core::EmbeddingProvider>>> {
    if config.provider == "disabled" {
        return Ok(None);
    }
    Ok(Some(Arc::new(embedding::HttpEmbedding::new(
        config, privacy,
    )?)))
}

/// Builds a reranker. Local inference and explicitly allowed downloads are lazy.
/// `fail_open` is intentionally handled by the engine, not by this adapter.
pub fn build_reranker(
    config: &RerankConfig,
    privacy: &PrivacyConfig,
) -> Result<Option<Arc<dyn core::RerankProvider>>> {
    match config.provider.as_str() {
        "disabled" => Ok(None),
        "remote" | "openai" | "dashscope" | "siliconflow" => {
            Ok(Some(Arc::new(rerank::HttpReranker::new(config, privacy)?)))
        }
        "fastembed" => {
            #[cfg(feature = "local-rerank")]
            {
                Ok(Some(Arc::new(local::LocalReranker::new(config)?)))
            }
            #[cfg(not(feature = "local-rerank"))]
            {
                Err(Error::Config(
                    "fastembed requires a build with feature `local-rerank` and an installed ONNX runtime via ORT_DYLIB_PATH".into(),
                ))
            }
        }
        _ => Err(Error::Config("unsupported rerank provider".into())),
    }
}

/// Validates a vector and returns its L2 normalization. Accumulation in f64
/// handles both very large and subnormal finite f32 values without overflow.
pub fn normalize_vector(vector: Vec<f32>, expected_dimensions: usize) -> Result<Vec<f32>> {
    if expected_dimensions == 0 || vector.len() != expected_dimensions {
        return Err(Error::ModelIncompatible(
            "embedding dimension mismatch".into(),
        ));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(Error::ModelIncompatible(
            "embedding contains NaN or infinity".into(),
        ));
    }
    let norm = vector
        .iter()
        .map(|&v| f64::from(v).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(Error::ModelIncompatible(
            "embedding has zero or invalid norm".into(),
        ));
    }
    Ok(vector
        .into_iter()
        .map(|v| (f64::from(v) / norm) as f32)
        .collect())
}

pub(crate) fn validate_timeout(seconds: u64) -> Result<()> {
    if !(1..=300).contains(&seconds) {
        return Err(Error::Config(
            "model timeout_secs must be in 1..=300".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_validation_and_numerical_extremes() {
        for vector in [
            vec![],
            vec![0.0, 0.0],
            vec![f32::NAN, 1.0],
            vec![f32::INFINITY, 1.0],
            vec![1.0],
        ] {
            assert!(normalize_vector(vector, 2).is_err());
        }
        for vector in [
            vec![3.0, 4.0],
            vec![f32::MAX, f32::MAX],
            vec![f32::from_bits(1), 0.0],
        ] {
            let normalized = normalize_vector(vector, 2).unwrap();
            let norm: f32 = normalized.iter().map(|v| v * v).sum();
            assert!((norm - 1.0).abs() < 1e-6);
        }
        assert!(normalize_vector(vec![], 0).is_err());
    }
}
