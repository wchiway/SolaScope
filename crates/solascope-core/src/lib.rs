use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub type Result<T, E = Error> = std::result::Result<T, E>;
pub const EMBEDDING_TEMPLATE_VERSION: &str = "code-v1";
pub const MAX_RESULTS: usize = 100;
pub const MAX_CONTEXT_LINES: usize = 200;
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("privacy boundary: {0}")]
    Privacy(String),
    #[error("index not found; run `solascope index` first")]
    IndexMissing,
    #[error("index needs upgrading: {0}; run `solascope index`")]
    IndexOutdated(String),
    #[error("another index writer is active; stop it or retry")]
    WriterBusy,
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("model/index incompatible: {0}; run `solascope index --rebuild`")]
    ModelIncompatible(String),
    #[error("model authentication or quota failure: {0}")]
    Authentication(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("parser error: {0}")]
    Parse(String),
    #[error("source changed during indexing: {0}; retry the scan")]
    SourceChanged(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::InvalidInput(_) | Self::Privacy(_) => 2,
            Self::IndexMissing | Self::IndexOutdated(_) => 3,
            Self::ModelUnavailable(_) | Self::ModelIncompatible(_) | Self::Authentication(_) => 4,
            Self::WriterBusy => 5,
            Self::Cancelled => 130,
            _ => 1,
        }
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn hash_text(text: &str) -> String {
    hash_bytes(text.as_bytes())
}

pub fn project_id(root: &Path) -> String {
    hash_text(&root.to_string_lossy())[..24].to_owned()
}

pub fn validate_relative_path(path: &str) -> Result<()> {
    let normalized = path.replace('\\', "/");
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\0')
        || normalized.starts_with('/')
        || normalized.contains(':')
        || Path::new(path).is_absolute()
        || normalized.split('/').any(|part| part == "..")
    {
        return Err(Error::InvalidInput(
            "paths must be project-relative and cannot escape the root".into(),
        ));
    }
    Ok(())
}

pub fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut output: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub relative_path: String,
    pub absolute_path: PathBuf,
    pub content: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub id: String,
    pub entity_id: String,
    pub project_id: String,
    pub relative_path: String,
    pub language: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub signature: Option<String>,
    pub parent_symbol: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub content: String,
    pub content_hash: String,
    pub embedding_text: String,
    pub embedding_hash: String,
    pub fallback: bool,
}

impl Chunk {
    /// Identifies the embedding input apart from its path. Two chunks with the
    /// same key differ only in where their file lives, so a moved or renamed
    /// file can reuse the vectors computed at its previous location.
    pub fn relocation_key(&self) -> String {
        hash_text(&format!(
            "{EMBEDDING_TEMPLATE_VERSION}\0{}\0{}\0{}\0{}",
            self.language,
            self.symbol.as_deref().unwrap_or(""),
            self.signature.as_deref().unwrap_or(""),
            self.content_hash
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ModelFingerprint {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub dimensions: usize,
    pub normalization: String,
    pub template_version: String,
}

impl ModelFingerprint {
    pub fn key(&self) -> String {
        hash_text(&format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            self.provider,
            self.model,
            self.endpoint,
            self.dimensions,
            self.normalization,
            self.template_version
        ))
    }
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn fingerprint(&self) -> ModelFingerprint;
    fn is_remote(&self) -> bool;
    /// Effective HTTP batch size, when known, for aligning bounded indexing groups.
    fn batch_size_hint(&self) -> Option<usize> {
        None
    }
    /// Embed index documents, keeping the input order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Providers with asymmetric query/document modes override this method.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut vectors = self.embed(&[query.to_owned()]).await?;
        if vectors.len() != 1 {
            return Err(Error::ModelIncompatible(
                "query embedding count must be one".into(),
            ));
        }
        Ok(vectors.remove(0))
    }
}

#[derive(Debug, Clone)]
pub struct RerankDocument {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankScore {
    pub document_id: String,
    pub index: usize,
    pub score: f64,
}

/// The largest request a reranker accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerankLimits {
    pub max_documents: usize,
    pub max_input_chars: usize,
}

#[async_trait]
pub trait RerankProvider: Send + Sync {
    fn name(&self) -> String;
    fn is_remote(&self) -> bool;
    fn limits(&self) -> RerankLimits;
    async fn rerank(&self, query: &str, documents: &[RerankDocument]) -> Result<Vec<RerankScore>>;
}

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Lexical,
    Semantic,
    #[default]
    Hybrid,
}

impl std::str::FromStr for SearchMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "lexical" => Ok(Self::Lexical),
            "semantic" => Ok(Self::Semantic),
            "hybrid" => Ok(Self::Hybrid),
            _ => Err(Error::InvalidInput(
                "mode must be lexical, semantic or hybrid".into(),
            )),
        }
    }
}

impl std::fmt::Display for SearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    pub query: String,
    pub mode: SearchMode,
    pub limit: usize,
    pub path_filter: Option<String>,
    pub rerank: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: SearchMode::Hybrid,
            limit: 10,
            path_filter: None,
            rerank: true,
        }
    }
}

impl SearchRequest {
    pub fn validate(&self) -> Result<()> {
        validate_query(&self.query, self.limit)?;
        if let Some(path) = &self.path_filter {
            validate_relative_path(path)?;
        }
        Ok(())
    }
}

pub fn validate_query(query: &str, limit: usize) -> Result<()> {
    if query.trim().is_empty() || query.len() > 4096 || query.contains('\0') {
        return Err(Error::InvalidInput(
            "query must contain 1–4096 bytes of text".into(),
        ));
    }
    if !(1..=MAX_RESULTS).contains(&limit) {
        return Err(Error::InvalidInput(format!(
            "limit must be between 1 and {MAX_RESULTS}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRequest {
    pub name: String,
    pub kind: Option<String>,
    pub path_filter: Option<String>,
    pub limit: usize,
}

impl SymbolRequest {
    pub fn validate(&self) -> Result<()> {
        validate_query(&self.name, self.limit)?;
        if let Some(path) = &self.path_filter {
            validate_relative_path(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RankedChunk {
    pub chunk: Chunk,
    pub file_hash: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub project_id: String,
    pub relative_path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub language: String,
    pub signature: Option<String>,
    pub parent_symbol: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
    pub score: f64,
    pub rank: usize,
    pub match_sources: Vec<String>,
    pub index_version: u64,
    pub stale: bool,
}

/// Wall-clock milliseconds per search stage. A model stage is zero when it did
/// not run or was served from the in-process query cache.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct SearchTimings {
    pub embedding_ms: f64,
    pub retrieval_ms: f64,
    pub rerank_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    pub mode: SearchMode,
    pub index_version: u64,
    pub results: Vec<SearchResult>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub timings: SearchTimings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextRequest {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub line: Option<usize>,
    pub before: usize,
    pub after: usize,
}

impl Default for ContextRequest {
    fn default() -> Self {
        Self {
            path: String::new(),
            start_line: None,
            end_line: None,
            line: None,
            before: 10,
            after: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextLine {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextResponse {
    pub relative_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub lines: Vec<ContextLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexGeneration {
    pub version: u64,
    pub fingerprint: Option<ModelFingerprint>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexStatus {
    pub project_id: String,
    pub root: String,
    pub index_dir: String,
    pub index_version: u64,
    pub files: usize,
    pub chunks: usize,
    pub vectors: usize,
    pub cache_entries: usize,
    /// Vectors reused from a previous path of the same code; `index --rebuild`
    /// recomputes them from the current path.
    pub relocated_vectors: usize,
    pub embedding_fingerprint: Option<ModelFingerprint>,
    pub last_indexed_at: Option<u64>,
    pub pending_jobs: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexReport {
    pub project_id: String,
    pub index_version: u64,
    pub files_total: usize,
    pub files_updated: usize,
    pub files_unchanged: usize,
    pub files_deleted: usize,
    pub chunks: usize,
    pub embedded_chunks: usize,
    pub cache_hits: usize,
    pub relocated_embeddings: usize,
    pub skipped_files: usize,
    pub warnings: Vec<String>,
    pub elapsed_ms: u128,
    pub rebuilt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCheck {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DiagnosticCheck>,
    pub healthy: bool,
}

pub trait MetadataStore {
    fn status(&self) -> Result<IndexStatus>;
}

pub trait LexicalIndex {
    fn lexical_search(
        &self,
        query: &str,
        limit: usize,
        path_filter: Option<&str>,
    ) -> Result<Vec<RankedChunk>>;
    fn symbol_search(&self, request: &SymbolRequest, fuzzy: bool) -> Result<Vec<RankedChunk>>;
}

pub trait VectorIndex {
    fn vector_search(
        &self,
        query: &[f32],
        fingerprint: &str,
        limit: usize,
        path_filter: Option<&str>,
    ) -> Result<Vec<RankedChunk>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_cross_platform_path_escapes() {
        for path in [
            "../secret",
            "a/../../secret",
            "/etc/passwd",
            "C:\\secret",
            "\\\\host\\share",
            "a\0b",
        ] {
            assert!(validate_relative_path(path).is_err(), "{path}");
        }
        assert!(validate_relative_path("src/**/*.rs").is_ok());
    }

    #[test]
    fn fingerprints_include_vector_space_and_template() {
        let base = ModelFingerprint {
            provider: "ollama".into(),
            model: "nomic-embed-text".into(),
            endpoint: "http://localhost:11434".into(),
            dimensions: 768,
            normalization: "l2".into(),
            template_version: EMBEDDING_TEMPLATE_VERSION.into(),
        };
        let mut changed = base.clone();
        changed.dimensions = 1024;
        assert_ne!(base.key(), changed.key());
        changed = base.clone();
        changed.template_version = "code-v2".into();
        assert_ne!(base.key(), changed.key());
    }

    #[test]
    fn unicode_truncation_preserves_boundaries() {
        assert_eq!(truncate_text("中文 Rust", 2), "中文…");
    }
}
