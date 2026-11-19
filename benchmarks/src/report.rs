//! Pieces shared by both reports: where the numbers were taken, which models
//! were configured, and message tallies.

use std::time::Duration;

use serde::Serialize;
use solascope_config::Config;

use crate::metrics::round_ms;

pub const DEBUG_BUILD_NOTICE: &str =
    "debug build: timings are not representative; use `cargo run --release` for numbers worth comparing";

/// Where and how the harness ran. Timings are only comparable between runs
/// that agree on all of this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Environment {
    pub harness_version: &'static str,
    /// `debug` when compiled with debug assertions (Cargo's dev profile),
    /// otherwise `release`.
    pub build_profile: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub logical_cpus: Option<usize>,
}

impl Environment {
    pub fn detect() -> Self {
        Self {
            harness_version: env!("CARGO_PKG_VERSION"),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism()
                .ok()
                .map(|count| count.get()),
        }
    }

    pub fn is_debug_build(&self) -> bool {
        self.build_profile == "debug"
    }

    pub fn describe_host(&self) -> String {
        match self.logical_cpus {
            Some(cpus) => format!("{} {}, {cpus} logical CPUs", self.os, self.arch),
            None => format!("{} {}", self.os, self.arch),
        }
    }

    pub fn describe_build(&self) -> String {
        if self.is_debug_build() {
            format!(
                "{} {} (unoptimized; timings are not representative)",
                self.harness_version, self.build_profile
            )
        } else {
            format!("{} {}", self.harness_version, self.build_profile)
        }
    }
}

/// The model configuration that decides which variants can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSummary {
    pub embedding_provider: String,
    /// `None` while the provider is disabled.
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<usize>,
    pub rerank_provider: String,
    /// `None` while the provider is disabled.
    pub rerank_model: Option<String>,
}

impl ModelSummary {
    pub fn from_config(config: &Config) -> Self {
        let embedding = config.embedding.provider != "disabled";
        let rerank = config.rerank.provider != "disabled";
        Self {
            embedding_provider: config.embedding.provider.clone(),
            embedding_model: embedding.then(|| config.embedding.model.clone()),
            embedding_dimensions: embedding.then_some(config.embedding.dimensions),
            rerank_provider: config.rerank.provider.clone(),
            rerank_model: rerank.then(|| config.rerank.model.clone()),
        }
    }

    pub fn embedding_enabled(&self) -> bool {
        self.embedding_model.is_some()
    }

    pub fn describe_embedding(&self) -> String {
        match (&self.embedding_model, self.embedding_dimensions) {
            (Some(model), Some(dimensions)) => format!(
                "{} / {model} / {dimensions} dimensions",
                self.embedding_provider
            ),
            _ => self.embedding_provider.clone(),
        }
    }

    pub fn describe_rerank(&self) -> String {
        match &self.rerank_model {
            Some(model) => format!("{} / {model}", self.rerank_provider),
            None => self.rerank_provider.clone(),
        }
    }
}

/// The effective configuration as JSON, for the record. Configuration holds no
/// secrets: API keys are referenced by environment variable name only.
pub fn config_json(config: &Config) -> serde_json::Value {
    serde_json::to_value(config).unwrap_or(serde_json::Value::Null)
}

/// A distinct message and the number of queries it was reported for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageCount {
    pub message: String,
    pub queries: usize,
}

/// Counts equal messages, keeping the order in which they first appeared.
pub fn tally<'a>(messages: impl IntoIterator<Item = &'a str>) -> Vec<MessageCount> {
    let mut counts: Vec<MessageCount> = Vec::new();
    for message in messages {
        match counts.iter_mut().find(|entry| entry.message == message) {
            Some(entry) => entry.queries += 1,
            None => counts.push(MessageCount {
                message: message.to_owned(),
                queries: 1,
            }),
        }
    }
    counts
}

/// Wall-clock milliseconds, rounded to whole microseconds.
pub fn millis(duration: Duration) -> f64 {
    round_ms(duration.as_secs_f64() * 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies_keep_first_seen_order() {
        let counts = tally(["b", "a", "b", "b", "c", "a"]);
        let pairs: Vec<(&str, usize)> = counts
            .iter()
            .map(|entry| (entry.message.as_str(), entry.queries))
            .collect();
        assert_eq!(pairs, [("b", 3), ("a", 2), ("c", 1)]);
        assert!(tally([]).is_empty());
    }

    #[test]
    fn durations_become_milliseconds_with_microsecond_resolution() {
        assert_eq!(millis(Duration::from_micros(1_500)), 1.5);
        assert_eq!(millis(Duration::from_nanos(1_234_567)), 1.235);
        assert_eq!(millis(Duration::ZERO), 0.0);
    }

    #[test]
    fn model_summary_hides_unused_model_names() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default_for_root(root.path()).unwrap();
        let disabled = ModelSummary::from_config(&config);
        assert!(!disabled.embedding_enabled());
        assert_eq!(disabled.describe_embedding(), "disabled");
        assert_eq!(disabled.describe_rerank(), "disabled");
        assert_eq!(disabled.embedding_model, None);

        config.embedding.provider = "ollama".into();
        config.rerank.provider = "remote".into();
        config.rerank.model = "rank-1".into();
        let enabled = ModelSummary::from_config(&config);
        assert!(enabled.embedding_enabled());
        assert_eq!(
            enabled.describe_embedding(),
            "ollama / nomic-embed-text / 768 dimensions"
        );
        assert_eq!(enabled.describe_rerank(), "remote / rank-1");
        assert_eq!(config_json(&config)["embedding"]["provider"], "ollama");
    }

    #[test]
    fn environment_reports_the_build_profile() {
        let environment = Environment::detect();
        assert_eq!(environment.is_debug_build(), cfg!(debug_assertions));
        assert!(environment.describe_host().contains(std::env::consts::OS));
        assert!(environment
            .describe_build()
            .starts_with(env!("CARGO_PKG_VERSION")));
    }
}
