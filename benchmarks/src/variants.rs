//! The retrieval variants under comparison and which of them a configuration
//! can run.

use serde::Serialize;
use solascope_config::Config;
use solascope_core::{SearchMode, SearchRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Variant {
    /// Lexical and symbol recall only.
    #[serde(rename = "lexical")]
    Lexical,
    /// Lexical, symbol and (when an embedding provider is enabled) vector
    /// recall, fused; reranking off.
    #[serde(rename = "hybrid")]
    Hybrid,
    /// Vector recall only.
    #[serde(rename = "semantic")]
    Semantic,
    /// [`Variant::Hybrid`] followed by the configured reranker.
    #[serde(rename = "hybrid+rerank")]
    HybridRerank,
}

impl Variant {
    /// Every variant, in the order they are run and reported.
    pub const ALL: [Self; 4] = [
        Self::Lexical,
        Self::Hybrid,
        Self::Semantic,
        Self::HybridRerank,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Hybrid => "hybrid",
            Self::Semantic => "semantic",
            Self::HybridRerank => "hybrid+rerank",
        }
    }

    pub fn mode(self) -> SearchMode {
        match self {
            Self::Lexical => SearchMode::Lexical,
            Self::Semantic => SearchMode::Semantic,
            Self::Hybrid | Self::HybridRerank => SearchMode::Hybrid,
        }
    }

    pub fn rerank(self) -> bool {
        self == Self::HybridRerank
    }

    pub fn request(self, query: &str, limit: usize) -> SearchRequest {
        SearchRequest {
            query: query.to_owned(),
            mode: self.mode(),
            limit,
            path_filter: None,
            rerank: self.rerank(),
        }
    }

    /// Why this variant cannot run under `config`, or `None` when it can.
    pub fn skip_reason(self, config: &Config) -> Option<String> {
        self.skip_reason_for_build(config, cfg!(feature = "local-rerank"))
    }

    fn skip_reason_for_build(self, config: &Config, local_rerank: bool) -> Option<String> {
        match self {
            Self::Lexical | Self::Hybrid => None,
            Self::Semantic => (config.embedding.provider == "disabled").then(|| {
                "embedding.provider is \"disabled\"; semantic search needs an embedding provider"
                    .to_owned()
            }),
            Self::HybridRerank => match config.rerank.provider.as_str() {
                "disabled" => {
                    Some("rerank.provider is \"disabled\"; no reranker is configured".to_owned())
                }
                "fastembed" if !local_rerank => Some(
                    "rerank.provider is \"fastembed\", but this harness was built without the `local-rerank` feature"
                        .to_owned(),
                ),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for Variant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedVariant {
    pub variant: Variant,
    pub reason: String,
}

/// Which variants to run and which to skip, each list in reporting order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub run: Vec<Variant>,
    pub skipped: Vec<SkippedVariant>,
}

pub fn plan(config: &Config) -> Plan {
    let mut plan = Plan {
        run: Vec::new(),
        skipped: Vec::new(),
    };
    for variant in Variant::ALL {
        match variant.skip_reason(config) {
            Some(reason) => plan.skipped.push(SkippedVariant { variant, reason }),
            None => plan.run.push(variant),
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let root = tempfile::tempdir().unwrap();
        let config = Config::default_for_root(root.path()).unwrap();
        (root, config)
    }

    #[test]
    fn without_models_only_lexical_and_hybrid_run() {
        let (_root, config) = config();
        let plan = plan(&config);
        assert_eq!(plan.run, [Variant::Lexical, Variant::Hybrid]);
        let skipped: Vec<Variant> = plan.skipped.iter().map(|skip| skip.variant).collect();
        assert_eq!(skipped, [Variant::Semantic, Variant::HybridRerank]);
        assert!(plan.skipped[0].reason.contains("embedding.provider"));
        assert!(plan.skipped[1].reason.contains("rerank.provider"));
    }

    #[test]
    fn enabled_providers_unlock_their_variants() {
        let (_root, mut config) = config();
        config.embedding.provider = "ollama".into();
        assert_eq!(
            plan(&config).run,
            [Variant::Lexical, Variant::Hybrid, Variant::Semantic]
        );
        config.rerank.provider = "remote".into();
        assert_eq!(plan(&config).run, Variant::ALL);
        assert!(plan(&config).skipped.is_empty());
        config.embedding.provider = "disabled".into();
        assert_eq!(
            plan(&config).run,
            [Variant::Lexical, Variant::Hybrid, Variant::HybridRerank]
        );
    }

    #[test]
    fn the_local_reranker_needs_the_matching_build() {
        let (_root, mut config) = config();
        config.rerank.provider = "fastembed".into();
        let reason = Variant::HybridRerank
            .skip_reason_for_build(&config, false)
            .unwrap();
        assert!(reason.contains("local-rerank"), "{reason}");
        assert_eq!(
            Variant::HybridRerank.skip_reason_for_build(&config, true),
            None
        );
    }

    #[test]
    fn requests_select_the_mode_and_reranking() {
        let expected = [
            (Variant::Lexical, SearchMode::Lexical, false, "lexical"),
            (Variant::Hybrid, SearchMode::Hybrid, false, "hybrid"),
            (Variant::Semantic, SearchMode::Semantic, false, "semantic"),
            (
                Variant::HybridRerank,
                SearchMode::Hybrid,
                true,
                "hybrid+rerank",
            ),
        ];
        for (variant, mode, rerank, name) in expected {
            let request = variant.request("find user", 7);
            assert_eq!(request.query, "find user");
            assert_eq!(request.mode, mode);
            assert_eq!(request.rerank, rerank);
            assert_eq!(request.limit, 7);
            assert_eq!(request.path_filter, None);
            assert_eq!(variant.to_string(), name);
            assert_eq!(serde_json::to_value(variant).unwrap(), name);
        }
    }
}
