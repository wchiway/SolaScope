//! Configuration precedence: CLI > process environment > project dotenv/TOML
//! > user dotenv/TOML > defaults. User defaults live in ~/.solascope.
//!
//! Every declared field supports that environment spelling (uppercase group and
//! field, e.g. `SOLASCOPE__EMBEDDING__PROVIDER=ollama`). Strings and paths are
//! literal values; booleans/numbers/arrays use TOML syntax, for example
//! `SOLASCOPE__PROJECT__EXCLUDE='["generated/**"]'`. Unknown variables in this
//! namespace are errors. Only API-key *environment variable names* are stored.
//! Relative file paths resolve beside their declaring TOML file; environment
//! paths and CLI paths resolve against the process working directory. Source-root
//! selection is separate: CLI root > process environment root > current directory;
//! TOML and dotenv root settings never redirect the source root.
use serde::{Deserialize, Serialize};
use solascope_core::{Error, Result, SearchMode};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

mod environment;
pub use environment::Environment;

macro_rules! section {
    ($name:ident { $($(#[$attribute:meta])* $field:ident : $ty:ty = $value:expr),* $(,)? }) => {
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name { $($(#[$attribute])* pub $field: $ty),* }
        impl Default for $name {
            fn default() -> Self { Self { $($field: $value),* } }
        }
    };
}

section!(ProjectConfig {
    root: PathBuf = PathBuf::from("."),
    index_dir: PathBuf = PathBuf::from(".solascope"),
    include: Vec<String> = vec![],
    exclude: Vec<String> = vec![],
    max_file_size: u64 = 1_048_576,
});
section!(ParserConfig {
    enabled_languages: Vec<String> = ["rust", "python", "typescript", "javascript", "go"].map(str::to_string).to_vec(),
    fallback_policy: String = "text".into(),
    max_chunk_tokens: usize = 512,
    overlap_lines: usize = 5,
});
section!(EmbeddingConfig {
    provider: String = "disabled".into(),
    model: String = "nomic-embed-text".into(),
    endpoint: String = "http://127.0.0.1:11434".into(),
    dimensions: usize = 768,
    request_dimensions: bool = true,
    api_key_env: String = "OPENAI_API_KEY".into(),
    #[serde(skip)]
    environment: Environment = Environment::default(),
    batch_size: usize = 32,
    concurrency: usize = 2,
    timeout_secs: u64 = 30,
    retries: usize = 2,
    retry_base_delay_ms: u64 = 1_000,
    retry_max_delay_ms: u64 = 60_000,
    rate_limit_cooldown_ms: u64 = 5_000,
    #[serde(skip_serializing_if = "Option::is_none")]
    requests_per_minute: Option<u32> = None,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_per_minute: Option<u64> = None,
    fingerprint_policy: String = "strict".into(),
});
section!(RerankConfig {
    provider: String = "disabled".into(),
    model: String = String::new(),
    endpoint: String = String::new(),
    api_key_env: String = "RERANK_API_KEY".into(),
    #[serde(skip)]
    environment: Environment = Environment::default(),
    top_n: usize = 20,
    timeout_secs: u64 = 30,
    fail_open: bool = true,
    max_input_chars: usize = 16_000,
    cache_dir: Option<PathBuf> = None,
    allow_model_download: bool = false,
});
section!(SearchConfig {
    mode: SearchMode = SearchMode::Hybrid,
    lexical_top_k: usize = 40,
    vector_top_k: usize = 40,
    symbol_top_k: usize = 20,
    fusion_k: f64 = 60.0,
    rerank_candidates: usize = 30,
    limit: usize = 10,
    semantic_fail_open: bool = true,
});
section!(IndexConfig {
    watch: bool = false,
    debounce_ms: u64 = 300,
    hash_algorithm: String = "sha256".into(),
    max_concurrent_jobs: usize = 4,
    snapshot_retention: usize = 2,
});
section!(PrivacyConfig {
    remote_opt_in: bool = false,
    sensitive_path_patterns: Vec<String> = vec![],
    symlink_policy: String = "skip".into(),
    allow_external_paths: bool = false,
});
section!(LogConfig {
    level: String = "info".into(),
    output: String = "stderr".into(),
    persist: bool = true,
    redact_secrets: bool = true,
    #[serde(skip)]
    directory: Option<PathBuf> = None,
});

impl EmbeddingConfig {
    /// Validate retry delays and optional provider rate budgets.
    pub fn validate_rate_limits(&self) -> Result<()> {
        if self.retry_base_delay_ms == 0 {
            return Err(invalid("embedding.retry_base_delay_ms must be positive"));
        }
        if self.retry_max_delay_ms == 0 || self.retry_max_delay_ms > 300_000 {
            return Err(invalid(
                "embedding.retry_max_delay_ms must be between 1 and 300000",
            ));
        }
        if self.rate_limit_cooldown_ms == 0 {
            return Err(invalid("embedding.rate_limit_cooldown_ms must be positive"));
        }
        if self.retry_base_delay_ms > self.retry_max_delay_ms {
            return Err(invalid(
                "embedding.retry_base_delay_ms must not exceed retry_max_delay_ms",
            ));
        }
        if self.rate_limit_cooldown_ms > self.retry_max_delay_ms {
            return Err(invalid(
                "embedding.rate_limit_cooldown_ms must not exceed retry_max_delay_ms",
            ));
        }
        if let Some(requests) = self.requests_per_minute {
            if requests == 0 {
                return Err(invalid("embedding.requests_per_minute must be positive"));
            }
        }
        if let Some(tokens) = self.tokens_per_minute {
            if tokens == 0 {
                return Err(invalid("embedding.tokens_per_minute must be positive"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub project: ProjectConfig,
    pub parser: ParserConfig,
    pub embedding: EmbeddingConfig,
    pub rerank: RerankConfig,
    pub search: SearchConfig,
    pub index: IndexConfig,
    pub privacy: PrivacyConfig,
    pub log: LogConfig,
}

impl Config {
    pub fn default_for_root(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(invalid("project.root must be a directory"));
        }
        let mut config = Self::default();
        config.project.index_dir = root.join(".solascope");
        config.project.root = root;
        config.validate()?;
        Ok(config)
    }

    /// The explicit log destination, or the reserved log beside the active configuration.
    pub fn diagnostic_log_path(&self) -> PathBuf {
        if self.log.output != "stderr" {
            PathBuf::from(&self.log.output)
        } else {
            self.log
                .directory
                .as_deref()
                .unwrap_or(&self.project.root)
                .join("logs/solascope.log")
        }
    }

    /// Load settings from the nearest project configuration or an explicit file.
    /// The source root comes from the CLI, the process environment or the current
    /// directory, never from configuration-file locations or root settings.
    pub fn load(root: Option<&Path>, config_path: Option<&Path>) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let env = std::env::vars_os()
            .filter(|(key, _)| key.to_string_lossy().starts_with("SOLASCOPE__"))
            .map(|(key, value)| {
                Ok((
                    key.into_string()
                        .map_err(|_| invalid("invalid environment variable name"))?,
                    value
                        .into_string()
                        .map_err(|_| invalid("configuration environment values must be UTF-8"))?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let global = global_config_path()?;
        load_with_sources(root, config_path, &cwd, Some(&global), &env)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.project.root.is_absolute() || !self.project.root.is_dir() {
            return Err(invalid(
                "project.root must be an absolute existing directory",
            ));
        }
        if !self.project.index_dir.is_absolute() {
            return Err(invalid("project.index_dir must be absolute"));
        }
        if self.project.root.starts_with(&self.project.index_dir) {
            return Err(invalid(
                "project.index_dir must not contain the project root",
            ));
        }
        if self.privacy.allow_external_paths {
            return Err(Error::Privacy(
                "allow_external_paths is unsupported in v0.1".into(),
            ));
        }
        one_of(
            &self.privacy.symlink_policy,
            &["skip", "within_root"],
            "privacy.symlink_policy",
        )?;
        one_of(
            &self.parser.fallback_policy,
            &["text", "skip"],
            "parser.fallback_policy",
        )?;
        one_of(
            &self.embedding.provider,
            &[
                "disabled",
                "ollama",
                "openai",
                "remote",
                "dashscope",
                "siliconflow",
            ],
            "embedding.provider",
        )?;
        one_of(
            &self.rerank.provider,
            &[
                "disabled",
                "remote",
                "openai",
                "dashscope",
                "siliconflow",
                "fastembed",
            ],
            "rerank.provider",
        )?;
        one_of(
            &self.log.level,
            &["off", "error", "warn", "info", "debug", "trace"],
            "log.level",
        )?;
        self.embedding.validate_rate_limits()?;
        one_of(
            &self.embedding.fingerprint_policy,
            &["strict", "rebuild"],
            "embedding.fingerprint_policy",
        )?;
        // The field names the revision hash so another algorithm can be added
        // without silently mixing hashes in one index.
        one_of(
            &self.index.hash_algorithm,
            &["sha256"],
            "index.hash_algorithm",
        )?;
        if self.log.output != "stderr" && !Path::new(&self.log.output).is_absolute() {
            return Err(invalid("log.output must be \"stderr\" or a file path"));
        }
        if self
            .log
            .directory
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(invalid("log configuration directory must be absolute"));
        }
        for (name, count) in [
            ("project.max_file_size", self.project.max_file_size),
            (
                "parser.max_chunk_tokens",
                self.parser.max_chunk_tokens as u64,
            ),
            ("embedding.dimensions", self.embedding.dimensions as u64),
            ("embedding.batch_size", self.embedding.batch_size as u64),
            ("embedding.concurrency", self.embedding.concurrency as u64),
            ("embedding.timeout_secs", self.embedding.timeout_secs),
            ("rerank.top_n", self.rerank.top_n as u64),
            ("rerank.timeout_secs", self.rerank.timeout_secs),
            ("rerank.max_input_chars", self.rerank.max_input_chars as u64),
            ("search.lexical_top_k", self.search.lexical_top_k as u64),
            ("search.vector_top_k", self.search.vector_top_k as u64),
            ("search.symbol_top_k", self.search.symbol_top_k as u64),
            (
                "search.rerank_candidates",
                self.search.rerank_candidates as u64,
            ),
            ("search.limit", self.search.limit as u64),
            (
                "index.max_concurrent_jobs",
                self.index.max_concurrent_jobs as u64,
            ),
            (
                "index.snapshot_retention",
                self.index.snapshot_retention as u64,
            ),
        ] {
            if count == 0 {
                return Err(invalid(&format!("{name} must be positive")));
            }
        }
        if self.embedding.timeout_secs > 300 {
            return Err(invalid("embedding.timeout_secs must not exceed 300"));
        }
        if self.embedding.retries > 5 {
            return Err(invalid("embedding.retries must not exceed 5"));
        }
        if !self.search.fusion_k.is_finite() || self.search.fusion_k <= 0.0 {
            return Err(invalid("search.fusion_k must be finite and positive"));
        }
        if self.search.limit > solascope_core::MAX_RESULTS {
            return Err(invalid("search.limit exceeds the supported result limit"));
        }
        for patterns in [
            &self.project.include,
            &self.project.exclude,
            &self.privacy.sensitive_path_patterns,
        ] {
            for pattern in patterns {
                globset::Glob::new(pattern).map_err(|_| invalid("invalid path glob"))?;
            }
        }
        validate_key_name(&self.embedding.api_key_env)?;
        validate_key_name(&self.rerank.api_key_env)?;
        let embedding_url = endpoint(&self.embedding.endpoint)?;
        let _ = endpoint(&self.rerank.endpoint)?;
        if self.embedding.provider != "disabled" && self.embedding.model.trim().is_empty() {
            return Err(invalid("embedding.model is required when enabled"));
        }
        if self.rerank.provider != "disabled" && self.rerank.model.trim().is_empty() {
            return Err(invalid("rerank.model is required when enabled"));
        }
        if self.embedding.provider != "disabled" && embedding_url.is_none() {
            return Err(invalid("embedding.endpoint is required when enabled"));
        }
        if !matches!(self.rerank.provider.as_str(), "disabled" | "fastembed")
            && self.rerank.endpoint.is_empty()
        {
            return Err(invalid("rerank.endpoint is required for remote reranking"));
        }
        let remote_ollama =
            self.embedding.provider == "ollama" && !embedding_url.as_ref().is_some_and(is_loopback);
        if !self.privacy.remote_opt_in
            && (!matches!(self.embedding.provider.as_str(), "disabled" | "ollama")
                || !matches!(self.rerank.provider.as_str(), "disabled" | "fastembed")
                || remote_ollama)
        {
            return Err(Error::Privacy(
                "remote model endpoints require privacy.remote_opt_in = true".into(),
            ));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> Error {
    Error::Config(message.into())
}

fn one_of(value: &str, allowed: &[&str], field: &str) -> Result<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(invalid(&format!("unsupported {field}")))
    }
}

fn validate_key_name(value: &str) -> Result<()> {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(invalid(
            "API credentials must be referenced by environment variable name",
        ));
    }
    Ok(())
}

/// Never propagate parser errors: URL error messages can contain credentials.
fn endpoint(value: &str) -> Result<Option<url::Url>> {
    if value.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(value).map_err(|_| invalid("invalid model endpoint URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "model endpoints require HTTP(S), a host, and no credentials, query or fragment",
        ));
    }
    Ok(Some(url))
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn absolute(path: &Path, base: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn discover_config_directory(start: &Path) -> PathBuf {
    for directory in start.ancestors() {
        if directory.join(".solascope.toml").is_file() || directory.join(".git").exists() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}

fn merge(target: &mut toml::Value, overlay: toml::Value) {
    match (target, overlay) {
        (toml::Value::Table(target), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = target.get_mut(&key) {
                    merge(existing, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, overlay) => *target = overlay,
    }
}

fn resolve_paths(value: &mut toml::Value, base: &Path) -> Result<()> {
    for (group, field) in [
        ("project", "root"),
        ("project", "index_dir"),
        ("rerank", "cache_dir"),
        ("log", "output"),
    ] {
        if let Some(path) = value.get_mut(group).and_then(|group| group.get_mut(field)) {
            let raw = path
                .as_str()
                .ok_or_else(|| invalid("configuration paths must be strings"))?;
            // `stderr` names the stream, not a file.
            if (group, field) == ("log", "output") && raw == "stderr" {
                continue;
            }
            *path = toml::Value::String(
                absolute(Path::new(raw), base)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(())
}

fn load_layer(path: &Path) -> Result<toml::Value> {
    let text = fs::read_to_string(path)?;
    let mut value: toml::Value = toml::from_str(&text)
        .map_err(|_| invalid("invalid TOML configuration (values redacted)"))?;
    resolve_paths(&mut value, path.parent().unwrap_or(Path::new(".")))?;
    Ok(value)
}

fn environment_layer(
    defaults: &toml::Value,
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<toml::Value> {
    let mut overlay = toml::Value::Table(toml::map::Map::new());
    for (key, raw) in env {
        let Some(suffix) = key.strip_prefix("SOLASCOPE__") else {
            continue;
        };
        let parts: Vec<_> = suffix.split("__").map(str::to_ascii_lowercase).collect();
        if parts.len() != 2 {
            return Err(invalid("expected SOLASCOPE__GROUP__FIELD"));
        }
        let field_default = defaults
            .get(&parts[0])
            .and_then(|group| group.get(&parts[1]));
        // Optional fields are omitted by TOML serialization; recognize their
        // environment spellings explicitly so values still receive strict typing.
        let is_path = parts[0] == "rerank" && parts[1] == "cache_dir";
        let optional_number = parts[0] == "embedding"
            && matches!(
                parts[1].as_str(),
                "requests_per_minute" | "tokens_per_minute"
            );
        let value = match field_default {
            Some(toml::Value::String(_)) => toml::Value::String(raw.clone()),
            None if is_path => toml::Value::String(raw.clone()),
            None if optional_number => {
                let wrapped: toml::Value = toml::from_str(&format!("value = {raw}"))
                    .map_err(|_| invalid("invalid TOML environment override (values redacted)"))?;
                let number = wrapped
                    .get("value")
                    .and_then(toml::Value::as_integer)
                    .filter(|number| *number >= 0)
                    .ok_or_else(|| {
                        invalid("invalid integer environment override (values redacted)")
                    })?;
                toml::Value::Integer(number)
            }
            Some(_) => {
                let wrapped: toml::Value = toml::from_str(&format!("value = {raw}"))
                    .map_err(|_| invalid("invalid TOML environment override (values redacted)"))?;
                wrapped
                    .get("value")
                    .cloned()
                    .ok_or_else(|| invalid("missing environment value"))?
            }
            None => return Err(invalid("unknown SOLASCOPE configuration field")),
        };
        let mut fields = toml::map::Map::new();
        fields.insert(parts[1].clone(), value);
        let mut group = toml::map::Map::new();
        group.insert(parts[0].clone(), toml::Value::Table(fields));
        merge(&mut overlay, toml::Value::Table(group));
    }
    resolve_paths(&mut overlay, cwd)?;
    Ok(overlay)
}

// Source injection keeps tests independent of process-global environment state.
fn load_with_sources(
    root: Option<&Path>,
    config_path: Option<&Path>,
    cwd: &Path,
    global: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<Config> {
    let explicit_root = root.map(|path| absolute(path, cwd)).or_else(|| {
        env.get("SOLASCOPE__PROJECT__ROOT")
            .map(|path| absolute(Path::new(path), cwd))
    });
    let selected = explicit_root.clone().unwrap_or_else(|| cwd.to_path_buf());
    let mut base = Config::default_for_root(&selected)?;
    let mut value =
        toml::Value::try_from(&base).map_err(|_| invalid("cannot serialize defaults"))?;
    let defaults = value.clone();
    let mut explicit_index = false;
    let mut environment = Environment::default();
    let mut log_directory = global
        .and_then(Path::parent)
        .unwrap_or(&selected)
        .to_path_buf();
    let project = config_path
        .map(|path| absolute(path, cwd))
        .unwrap_or_else(|| {
            explicit_root
                .unwrap_or_else(|| discover_config_directory(cwd))
                .join(".solascope.toml")
        });
    let sources = global
        .into_iter()
        .map(|path| (path, false, true))
        .chain(std::iter::once((
            project.as_path(),
            config_path.is_some(),
            false,
        )));
    for (path, required, user_defaults) in sources {
        let present = match fs::metadata(path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => false,
            Err(error) => return Err(error.into()),
        };
        if present {
            log_directory = path.parent().unwrap_or(&selected).to_path_buf();
            let layer = load_layer(path)?;
            explicit_index |= layer
                .get("project")
                .and_then(|v| v.get("index_dir"))
                .is_some();
            merge(&mut value, layer);
        }
        if present || user_defaults {
            let directory = path.parent().unwrap_or(&selected);
            let file_environment = Environment::load(directory)?;
            let overrides = file_environment
                .file_values()
                .iter()
                .filter(|(name, _)| {
                    name.starts_with("SOLASCOPE__")
                        && !env
                            .keys()
                            .any(|existing| existing.eq_ignore_ascii_case(name))
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let layer = environment_layer(&defaults, &overrides, directory)?;
            explicit_index |= layer
                .get("project")
                .and_then(|v| v.get("index_dir"))
                .is_some();
            merge(&mut value, layer);
            environment = environment.overlay(&file_environment);
        }
    }
    let layer = environment_layer(&defaults, env, cwd)?;
    explicit_index |= layer
        .get("project")
        .and_then(|v| v.get("index_dir"))
        .is_some();
    merge(&mut value, layer);
    // Configuration files supply settings, not source-directory selection.
    value["project"]["root"] = toml::Value::String(selected.to_string_lossy().into_owned());
    base = value.try_into().map_err(|_| {
        invalid("configuration has unknown fields or invalid field types (values redacted)")
    })?;
    base.project.root = fs::canonicalize(&base.project.root)?;
    if !explicit_index {
        base.project.index_dir = base.project.root.join(".solascope");
    }
    base.embedding.environment = environment.clone();
    base.rerank.environment = environment;
    base.log.directory = Some(absolute(&log_directory, cwd));
    base.validate()?;
    Ok(base)
}

/// Shared user configuration, independent of any project root.
pub fn user_config_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir);
    #[cfg(not(windows))]
    let home = dirs::home_dir();
    let home = home.ok_or_else(|| invalid("cannot determine the user home directory"))?;
    Ok(home.join(".solascope/.solascope.toml"))
}

fn global_config_path() -> Result<PathBuf> {
    let path = user_config_path()?;
    if !path.try_exists()? {
        if let Some(legacy) =
            dirs::config_dir().map(|directory| directory.join("solascope/config.toml"))
        {
            if legacy.try_exists()? {
                return Ok(legacy);
            }
        }
    }
    Ok(path)
}

/// Create user defaults without binding subsequent commands to this directory.
/// Existing configurations are never truncated and providers stay disabled.
pub fn write_default_config(directory: &Path) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    let mut template = toml::Value::try_from(Config::default())
        .map_err(|_| invalid("cannot render default configuration"))?;
    let project = template["project"]
        .as_table_mut()
        .ok_or_else(|| invalid("invalid default project settings"))?;
    project.remove("root");
    project.remove("index_dir");
    let body = toml::to_string_pretty(&template)
        .map_err(|_| invalid("cannot render default configuration"))?;
    let path = directory.join(".solascope.toml");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(b"# User defaults; project root and index directory are determined per command.\n# Models are disabled. API keys belong in the neighboring .env or process environment.\n")?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn load(root: &Path, global: Option<&Path>, env: &[(&str, &str)]) -> Result<Config> {
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        load_with_sources(None, None, root, global, &env)
    }

    #[test]
    fn diagnostic_logs_follow_the_selected_configuration_directory() {
        let source = tempfile::tempdir().unwrap();
        let settings = tempfile::tempdir().unwrap();
        let global = settings.path().join("user/.solascope.toml");
        let config = load_with_sources(
            Some(source.path()),
            None,
            source.path(),
            Some(&global),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            config.diagnostic_log_path(),
            settings.path().join("user/logs/solascope.log")
        );
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(&global, "[log]\nlevel='debug'\n").unwrap();
        let config = load_with_sources(
            Some(source.path()),
            None,
            source.path(),
            Some(&global),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            config.diagnostic_log_path(),
            settings.path().join("user/logs/solascope.log")
        );
        fs::write(source.path().join(".solascope.toml"), "[search]\nlimit=3\n").unwrap();
        let config = load_with_sources(
            Some(source.path()),
            None,
            source.path(),
            Some(&global),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            config.diagnostic_log_path(),
            source.path().join("logs/solascope.log")
        );
        let explicit = settings.path().join("explicit.toml");
        fs::write(&explicit, "[log]\nlevel='info'\n").unwrap();
        let config = load_with_sources(
            Some(source.path()),
            Some(&explicit),
            source.path(),
            Some(&global),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            config.diagnostic_log_path(),
            settings.path().join("logs/solascope.log")
        );
        assert_eq!(config.project.root, source.path().canonicalize().unwrap());
        let serialized = toml::Value::try_from(&config).unwrap();
        assert!(serialized["log"].get("directory").is_none());
    }

    #[test]
    fn automatic_log_persistence_can_be_disabled_without_changing_explicit_output() {
        let root = tempfile::tempdir().unwrap();
        let environment =
            BTreeMap::from([("SOLASCOPE__LOG__PERSIST".to_owned(), "false".to_owned())]);
        let config =
            load_with_sources(Some(root.path()), None, root.path(), None, &environment).unwrap();
        assert!(!config.log.persist);
        assert_eq!(
            config.diagnostic_log_path(),
            root.path().join("logs/solascope.log")
        );
        fs::write(
            root.path().join(".solascope.toml"),
            "[log]\noutput='custom.log'\npersist=false\n",
        )
        .unwrap();
        let config = load(root.path(), None, &[]).unwrap();
        assert_eq!(config.diagnostic_log_path(), root.path().join("custom.log"));
    }

    #[test]
    fn defaults_and_atomic_init_preserve_existing_file() {
        let root = tempdir().unwrap();
        let config = Config::default_for_root(root.path()).unwrap();
        assert!(!config.privacy.remote_opt_in);
        assert!(!config.privacy.allow_external_paths);
        assert!(!config.rerank.allow_model_download);
        assert_eq!(config.embedding.provider, "disabled");
        assert_eq!(config.rerank.provider, "disabled");
        assert_eq!(config.project.index_dir, root.path().join(".solascope"));
        let path = write_default_config(root.path()).unwrap();
        let first = fs::read(&path).unwrap();
        assert!(write_default_config(root.path()).is_err());
        assert_eq!(first, fs::read(&path).unwrap());
        assert_eq!(
            load(root.path(), None, &[]).unwrap().embedding.provider,
            "disabled"
        );
    }

    #[test]
    fn merges_all_layers_and_resolves_paths_at_declaration() {
        let root = tempdir().unwrap();
        let global_dir = tempdir().unwrap();
        let global = global_dir.path().join("config.toml");
        fs::write(
            &global,
            "[search]\nlimit=3\nlexical_top_k=8\n[rerank]\ncache_dir='cache'\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[search]\nlimit=4\n[project]\nindex_dir='local-index'\n",
        )
        .unwrap();
        let config = load(
            root.path(),
            Some(&global),
            &[("SOLASCOPE__SEARCH__LIMIT", "7")],
        )
        .unwrap();
        assert_eq!(config.search.limit, 7);
        assert_eq!(config.search.lexical_top_k, 8);
        assert_eq!(config.project.index_dir, root.path().join("local-index"));
        assert_eq!(
            config.rerank.cache_dir,
            Some(global_dir.path().join("cache"))
        );
        assert_eq!(config.embedding.dimensions, 768);
    }

    #[test]
    fn configuration_file_roots_do_not_select_sources() {
        let root = tempdir().unwrap();
        let settings = tempdir().unwrap();
        let global = settings.path().join("config.toml");
        fs::write(
            &global,
            "[project]\nroot='missing-user-source'\n[search]\nlexical_top_k=8\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[project]\nroot='missing-project-source'\n[search]\nlimit=5\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".env"),
            "SOLASCOPE__PROJECT__ROOT=missing-dotenv-source\nSOLASCOPE__SEARCH__LIMIT=7\n",
        )
        .unwrap();
        let config = load(root.path(), Some(&global), &[]).unwrap();
        assert_eq!(config.project.root, root.path().canonicalize().unwrap());
        assert_eq!(
            config.project.index_dir,
            config.project.root.join(".solascope")
        );
        assert_eq!(config.search.limit, 7);
        assert_eq!(config.search.lexical_top_k, 8);
    }

    #[test]
    fn cli_root_beats_environment_and_root_moves_default_index() {
        let cwd = tempdir().unwrap();
        let cli = tempdir().unwrap();
        let env_root = tempdir().unwrap();
        let env = BTreeMap::from([(
            "SOLASCOPE__PROJECT__ROOT".into(),
            env_root.path().to_string_lossy().into(),
        )]);
        let config = load_with_sources(Some(cli.path()), None, cwd.path(), None, &env).unwrap();
        assert_eq!(config.project.root, cli.path());
        assert_eq!(config.project.index_dir, cli.path().join(".solascope"));
        let config = load_with_sources(None, None, cwd.path(), None, &env).unwrap();
        assert_eq!(config.project.root, env_root.path());
        assert_eq!(config.project.index_dir, env_root.path().join(".solascope"));
    }

    #[test]
    fn discovers_parent_configuration_without_expanding_the_source_root() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("src/deep");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        let config = load(&cwd, None, &[]).unwrap();
        assert_eq!(config.project.root, cwd.canonicalize().unwrap());
        assert_eq!(
            config.project.index_dir,
            config.project.root.join(".solascope")
        );
        let path = root.path().join(".solascope.toml");
        fs::write(&path, "[project]\nroot='src'\n[search]\nlimit=6\n").unwrap();
        for (selected, config_path) in [(None, None), (Some(cwd.as_path()), Some(path.as_path()))] {
            let config =
                load_with_sources(selected, config_path, &cwd, None, &BTreeMap::new()).unwrap();
            assert_eq!(config.project.root, cwd.canonicalize().unwrap());
            assert_eq!(
                config.project.index_dir,
                config.project.root.join(".solascope")
            );
            assert_eq!(config.search.limit, 6);
        }
    }

    #[test]
    fn explicit_roots_do_not_inherit_unrelated_parent_configuration() {
        let root = tempdir().unwrap();
        let selected = root.path().join("project");
        fs::create_dir(&selected).unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[project]\nindex_dir='shared-index'\n[search]\nlimit=1\n",
        )
        .unwrap();
        let env = BTreeMap::from([(
            "SOLASCOPE__PROJECT__ROOT".into(),
            selected.to_string_lossy().into_owned(),
        )]);
        for (cli_root, environment) in [(Some(selected.as_path()), BTreeMap::new()), (None, env)] {
            let config =
                load_with_sources(cli_root, None, root.path(), None, &environment).unwrap();
            assert_eq!(config.project.root, selected.canonicalize().unwrap());
            assert_eq!(
                config.project.index_dir,
                config.project.root.join(".solascope")
            );
            assert_eq!(config.search.limit, Config::default().search.limit);
        }
    }

    #[test]
    fn unknown_fields_and_invalid_parameters_are_rejected() {
        let root = tempdir().unwrap();
        for body in [
            "[project]\nunknown=1",
            "[unknown]\nx=1",
            "[search]\nlimit=0",
            "[search]\nfusion_k=nan",
            "[parser]\nfallback_policy='bad'",
            "[privacy]\nallow_external_paths=true",
        ] {
            fs::write(root.path().join(".solascope.toml"), body).unwrap();
            assert!(load(root.path(), None, &[]).is_err(), "{body}");
        }
        fs::remove_file(root.path().join(".solascope.toml")).unwrap();
        assert!(load(root.path(), None, &[("SOLASCOPE__PROJECT__TYPO", "1")]).is_err());
        assert!(load(
            root.path(),
            None,
            &[("SOLASCOPE__SEARCH__LIMIT", "invalid")]
        )
        .is_err());
        assert!(load(
            root.path(),
            None,
            &[("SOLASCOPE__PROJECT__EXCLUDE", "['secret/**']")]
        )
        .is_ok());
    }

    #[test]
    fn remote_providers_and_non_loopback_ollama_require_opt_in() {
        let root = tempdir().unwrap();
        let mut config = Config::default_for_root(root.path()).unwrap();
        config.embedding.provider = "ollama".into();
        for address in [
            "http://127.0.0.1:11434",
            "http://[::1]:11434",
            "http://localhost:11434",
        ] {
            config.embedding.endpoint = address.into();
            config.validate().unwrap();
        }
        for address in [
            "https://example.com",
            "http://localhost.example.com",
            "http://192.168.1.2:11434",
        ] {
            config.embedding.endpoint = address.into();
            assert!(matches!(config.validate(), Err(Error::Privacy(_))));
        }
        config.privacy.remote_opt_in = true;
        config.validate().unwrap();
        config.embedding.provider = "openai".into();
        config.privacy.remote_opt_in = false;
        assert!(matches!(config.validate(), Err(Error::Privacy(_))));
        config.embedding.provider = "disabled".into();
        config.rerank.provider = "remote".into();
        config.rerank.model = "example".into();
        config.rerank.endpoint = "https://example.com".into();
        assert!(matches!(config.validate(), Err(Error::Privacy(_))));
    }

    #[test]
    fn explicit_config_paths_preserve_cwd_and_environment_roots_remain_explicit() {
        let cwd = tempdir().unwrap();
        let project = tempdir().unwrap();

        fs::write(
            project.path().join("custom.toml"),
            "[project]\nroot='source'\nindex_dir='index'\n",
        )
        .unwrap();
        let config = load_with_sources(
            None,
            Some(&project.path().join("custom.toml")),
            cwd.path(),
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(config.project.root, cwd.path().canonicalize().unwrap());
        assert_eq!(config.project.index_dir, project.path().join("index"));
        assert!(load_with_sources(
            None,
            Some(Path::new("missing.toml")),
            cwd.path(),
            None,
            &BTreeMap::new()
        )
        .is_err());
        fs::write(
            project.path().join(".solascope.toml"),
            "[search]\nlimit=9\n",
        )
        .unwrap();
        let env = BTreeMap::from([(
            "SOLASCOPE__PROJECT__ROOT".into(),
            project.path().to_string_lossy().into_owned(),
        )]);
        let config = load_with_sources(None, None, cwd.path(), None, &env).unwrap();
        assert_eq!(config.project.root, project.path());
        assert_eq!(config.search.limit, 9);
    }

    #[test]
    fn credential_urls_are_rejected_without_echoing_secrets() {
        let root = tempdir().unwrap();
        let mut config = Config::default_for_root(root.path()).unwrap();
        for endpoint in [
            "https://user:secret@example.com",
            "https://example.com?api_key=secret",
            "https://example.com#secret",
        ] {
            config.embedding.endpoint = endpoint.into();
            let error = config.validate().unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn cloud_provider_names_are_valid_but_never_bypass_remote_consent() {
        let root = tempdir().unwrap();
        for provider in ["openai", "remote", "dashscope", "siliconflow"] {
            let mut config = Config::default_for_root(root.path()).unwrap();
            config.embedding.provider = provider.into();
            config.embedding.model = "configured-model".into();
            config.embedding.endpoint = "http://127.0.0.1:9000/v1".into();
            assert!(matches!(config.validate(), Err(Error::Privacy(_))));
            config.privacy.remote_opt_in = true;
            config.validate().unwrap();
            config.embedding.endpoint.clear();
            assert!(matches!(config.validate(), Err(Error::Config(_))));

            let mut config = Config::default_for_root(root.path()).unwrap();
            config.rerank.provider = provider.into();
            config.rerank.model = "configured-reranker".into();
            config.rerank.endpoint = "http://127.0.0.1:9000/rerank".into();
            assert!(matches!(config.validate(), Err(Error::Privacy(_))));
            config.privacy.remote_opt_in = true;
            config.validate().unwrap();
            config.rerank.endpoint.clear();
            assert!(matches!(config.validate(), Err(Error::Config(_))));
        }
    }

    #[test]
    fn watch_log_and_embedding_request_fields_obey_layer_precedence() {
        let root = tempdir().unwrap();
        fs::write(root.path().join(".solascope.toml"),
            "[log]\nlevel='debug'\noutput='logs/diagnostics.txt'\nredact_secrets=false\n[index]\nwatch=true\nhash_algorithm='sha256'\n[embedding]\nrequest_dimensions=false\nfingerprint_policy='rebuild'\n").unwrap();
        let config = load(root.path(), None, &[]).unwrap();
        assert_eq!(
            config.log.output,
            root.path().join("logs/diagnostics.txt").to_string_lossy()
        );
        assert!(!config.log.redact_secrets);
        assert!(config.index.watch);
        assert!(!config.embedding.request_dimensions);
        assert_eq!(config.embedding.fingerprint_policy, "rebuild");
        let config = load(
            root.path(),
            None,
            &[
                ("SOLASCOPE__LOG__OUTPUT", "run.log"),
                ("SOLASCOPE__LOG__REDACT_SECRETS", "true"),
                ("SOLASCOPE__INDEX__WATCH", "false"),
                ("SOLASCOPE__EMBEDDING__REQUEST_DIMENSIONS", "true"),
                ("SOLASCOPE__EMBEDDING__FINGERPRINT_POLICY", "strict"),
            ],
        )
        .unwrap();
        assert_eq!(
            config.log.output,
            root.path().join("run.log").to_string_lossy()
        );
        assert!(config.log.redact_secrets);
        assert!(!config.index.watch);
        assert!(config.embedding.request_dimensions);
        assert_eq!(config.embedding.fingerprint_policy, "strict");
    }

    #[test]
    fn unsupported_revision_hash_policy_and_mistyped_flags_are_rejected() {
        let root = tempdir().unwrap();
        for body in [
            "[index]\nhash_algorithm='blake3'",
            "[index]\nwatch='true'",
            "[embedding]\nfingerprint_policy='ignore'",
            "[embedding]\nrequest_dimensions='false'",
            "[log]\nlevel='verbose'",
            "[log]\nredact_secrets='false'",
        ] {
            fs::write(root.path().join(".solascope.toml"), body).unwrap();
            assert!(load(root.path(), None, &[]).is_err(), "{body}");
        }
    }

    #[test]
    fn user_defaults_do_not_bind_commands_to_the_configuration_directory() {
        let settings = tempdir().unwrap();
        let path = write_default_config(&settings.path().join("home/.solascope")).unwrap();
        let template: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(template["project"].get("root").is_none());
        assert!(template["project"].get("index_dir").is_none());
        for _ in 0..2 {
            let root = tempdir().unwrap();
            fs::create_dir(root.path().join(".git")).unwrap();
            let config =
                load_with_sources(None, None, root.path(), Some(&path), &BTreeMap::new()).unwrap();
            assert_eq!(config.project.root, root.path().canonicalize().unwrap());
            assert_eq!(
                config.project.index_dir,
                config.project.root.join(".solascope")
            );
        }
    }

    #[test]
    fn explicit_config_reads_its_own_dotenv_not_the_working_directory() {
        let root = tempdir().unwrap();
        let settings = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let path = settings.path().join("models.toml");
        let key = "SOLASCOPE_CONFIG_DOTENV_KEY_3D40";
        let before = std::env::var_os(key);
        fs::write(&path, format!("[embedding]\napi_key_env='{key}'\n[rerank]\napi_key_env='{key}'\n[search]\nlimit=4\n")).unwrap();
        fs::write(settings.path().join(".env"), format!(
            "export {key}='file-secret-value'\nSOLASCOPE__SEARCH__LIMIT=7\nSOLASCOPE__PROJECT__INDEX_DIR=cache\n"
        )).unwrap();
        fs::write(
            cwd.path().join(".env"),
            format!("{key}=wrong-directory\nSOLASCOPE__SEARCH__LIMIT=99\n"),
        )
        .unwrap();
        let config = load_with_sources(
            Some(root.path()),
            Some(&path),
            cwd.path(),
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(config.search.limit, 7);
        assert_eq!(config.project.index_dir, settings.path().join("cache"));
        assert_eq!(
            config.embedding.environment.file_values()[key],
            "file-secret-value"
        );
        assert_eq!(
            config.rerank.environment.file_values()[key],
            "file-secret-value"
        );
        assert_eq!(std::env::var_os(key), before);
        for rendered in [format!("{config:?}"), toml::to_string(&config).unwrap()] {
            assert!(!rendered.contains("file-secret-value"));
            assert!(!rendered.contains("wrong-directory"));
        }
    }

    #[test]
    fn global_project_dotenv_and_process_layers_have_explicit_precedence() {
        let root = tempdir().unwrap();
        let settings = tempdir().unwrap();
        let global = settings.path().join("user.toml");
        let key = "SOLASCOPE_CONFIG_LAYER_KEY_93D7";
        fs::write(&global, "[search]\nlimit=3\nlexical_top_k=8\n").unwrap();
        fs::write(settings.path().join(".env"), format!("{key}=global-key\nSOLASCOPE__SEARCH__LIMIT=4\nSOLASCOPE__SEARCH__LEXICAL_TOP_K=9\nSOLASCOPE__PRIVACY__REMOTE_OPT_IN=true\n")).unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[search]\nlimit=5\n[privacy]\nremote_opt_in=false\n",
        )
        .unwrap();
        let project = load(root.path(), Some(&global), &[]).unwrap();
        assert_eq!(project.search.limit, 5);
        assert_eq!(project.search.lexical_top_k, 9);
        assert!(!project.privacy.remote_opt_in);
        assert_eq!(
            project.embedding.environment.file_values()[key],
            "global-key"
        );
        fs::write(
            root.path().join(".env"),
            format!("{key}=project-key\nSOLASCOPE__SEARCH__LIMIT=6\n"),
        )
        .unwrap();
        let project = load(root.path(), Some(&global), &[]).unwrap();
        assert_eq!(project.search.limit, 6);
        assert_eq!(
            project.embedding.environment.file_values()[key],
            "project-key"
        );
        let process = load(
            root.path(),
            Some(&global),
            &[("SOLASCOPE__SEARCH__LIMIT", "7")],
        )
        .unwrap();
        assert_eq!(process.search.limit, 7);
        assert_eq!(
            process.embedding.environment.file_values()[key],
            "project-key"
        );
    }

    #[test]
    fn dotenv_credentials_are_isolated_between_configurations() {
        let key = "SOLASCOPE_CONFIG_ISOLATED_KEY_8412";
        let mut environments = Vec::new();
        let before = std::env::var_os(key);
        for value in ["first-project-key", "second-project-key"] {
            let root = tempdir().unwrap();
            fs::write(root.path().join(".solascope.toml"), "[search]\nlimit=2\n").unwrap();
            fs::write(root.path().join(".env"), format!("{key}={value}\n")).unwrap();
            let config = load(root.path(), None, &[]).unwrap();
            environments.push(config.embedding.environment);
        }
        assert_eq!(environments[0].file_values()[key], "first-project-key");
        assert_eq!(environments[1].file_values()[key], "second-project-key");
        assert_eq!(std::env::var_os(key), before);
    }

    #[test]
    fn dotenv_settings_reject_invalid_values_without_echoing_them() {
        let root = tempdir().unwrap();
        fs::write(root.path().join(".solascope.toml"), "").unwrap();
        fs::write(
            root.path().join(".env"),
            "SOLASCOPE__SEARCH__LIMIT='secret-invalid-limit'\n",
        )
        .unwrap();
        let error = load(root.path(), None, &[]).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret-invalid-limit"));
        let config = load(root.path(), None, &[("SOLASCOPE__SEARCH__LIMIT", "8")]).unwrap();
        assert_eq!(config.search.limit, 8);
    }

    #[test]
    fn an_unrelated_working_directory_dotenv_is_not_loaded_without_project_config() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(
            root.path().join(".env"),
            "invalid secret line without an equals sign\n",
        )
        .unwrap();
        let config = load_with_sources(None, None, root.path(), None, &BTreeMap::new()).unwrap();
        assert_eq!(config.embedding.provider, "disabled");
    }

    #[test]
    fn embedding_rate_limits_keep_defaults_and_obey_configuration_layer_precedence() {
        let root = tempdir().unwrap();
        let settings = tempdir().unwrap();
        let global = settings.path().join("user.toml");
        fs::write(
            &global,
            "[embedding]\nrequests_per_minute=11\ntokens_per_minute=1100\n",
        )
        .unwrap();
        fs::write(settings.path().join(".env"), "SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE=12\nSOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE=1200\n").unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[embedding]\nretry_base_delay_ms=250\nrequests_per_minute=21\ntokens_per_minute=2100\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".env"),
            "SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE=22\nSOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE=2200\n",
        )
        .unwrap();

        let config = load(root.path(), Some(&global), &[]).unwrap();
        assert_eq!(config.embedding.retry_base_delay_ms, 250);
        assert_eq!(config.embedding.retry_max_delay_ms, 60_000);
        assert_eq!(config.embedding.rate_limit_cooldown_ms, 5_000);
        assert_eq!(config.embedding.requests_per_minute, Some(22));
        assert_eq!(config.embedding.tokens_per_minute, Some(2_200));

        let process = load(
            root.path(),
            Some(&global),
            &[
                ("SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE", "31"),
                ("SOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE", "3100"),
            ],
        )
        .unwrap();
        assert_eq!(process.embedding.requests_per_minute, Some(31));
        assert_eq!(process.embedding.tokens_per_minute, Some(3_100));

        fs::write(root.path().join(".solascope.toml"), "[embedding]\n").unwrap();
        fs::remove_file(root.path().join(".env")).unwrap();
        fs::write(
            root.path().join(".solascope.toml"),
            "[embedding]\nretries=0\n",
        )
        .unwrap();
        let no_retries = load(root.path(), None, &[]).unwrap();
        assert_eq!(no_retries.embedding.retries, 0);

        fs::write(root.path().join(".solascope.toml"), "[embedding]\n").unwrap();
        let legacy = load(root.path(), None, &[]).unwrap();
        assert_eq!(legacy.embedding.retry_base_delay_ms, 1_000);
        assert_eq!(legacy.embedding.retry_max_delay_ms, 60_000);
        assert_eq!(legacy.embedding.rate_limit_cooldown_ms, 5_000);
        assert_eq!(legacy.embedding.requests_per_minute, None);
        assert_eq!(legacy.embedding.tokens_per_minute, None);
        let serialized = toml::to_string(&legacy).unwrap();
        assert!(!serialized.contains("requests_per_minute"));
        assert!(!serialized.contains("tokens_per_minute"));
    }

    #[test]
    fn embedding_rate_limits_reject_invalid_combinations_and_environment_types() {
        let root = tempdir().unwrap();
        let cases = [
            "retry_base_delay_ms=0",
            "retry_base_delay_ms=60001",
            "retry_max_delay_ms=0",
            "retry_max_delay_ms=300001",
            "rate_limit_cooldown_ms=0",
            "rate_limit_cooldown_ms=60001",
            "requests_per_minute=0",
            "requests_per_minute=-1",
            "tokens_per_minute=0",
            "tokens_per_minute=-1",
            "requests_per_minute=4294967296",
            "tokens_per_minute=18446744073709551616",
            "timeout_secs=301",
            "retries=6",
        ];
        for setting in cases {
            fs::write(
                root.path().join(".solascope.toml"),
                format!("[embedding]\n{setting}\n"),
            )
            .unwrap();
            assert!(load(root.path(), None, &[]).is_err(), "accepted {setting}");
        }
        fs::write(root.path().join(".solascope.toml"), "[embedding]\n").unwrap();
        for (name, raw) in [
            ("SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE", "true"),
            ("SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE", "0"),
            ("SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE", "-1"),
            ("SOLASCOPE__EMBEDDING__REQUESTS_PER_MINUTE", "4294967296"),
            ("SOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE", "0"),
            ("SOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE", "-1"),
            (
                "SOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE",
                "18446744073709551616",
            ),
            (
                "SOLASCOPE__EMBEDDING__TOKENS_PER_MINUTE",
                "not-a-number-secret",
            ),
            ("SOLASCOPE__EMBEDDING__UNKNOWN_RATE", "4"),
        ] {
            let error = load(root.path(), None, &[(name, raw)]).unwrap_err();
            assert!(!format!("{error:?} {error}").contains(raw));
        }
    }
}
