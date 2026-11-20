use solascope_config::Config;
use solascope_core::{
    project_id, truncate_text, Chunk, ContextLine, ContextRequest, ContextResponse,
    DiagnosticCheck, DoctorReport, Error, IndexReport, IndexStatus, LexicalIndex, MetadataStore,
    RankedChunk, RerankDocument, RerankScore, Result, SearchMode, SearchRequest, SearchResponse,
    SearchResult, SearchTimings, SourceFile, SymbolRequest, VectorIndex, MAX_CONTEXT_BYTES,
    MAX_CONTEXT_LINES, MAX_RESULTS,
};
use solascope_models::{build_embedding, build_reranker};
use solascope_parser::ParserOptions;
use solascope_storage::{CachedEmbedding, FileUpdate, IndexCommit, Store, WriterLock};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

type QueryCache = HashMap<String, (Instant, Vec<f32>)>;

type EmbeddingCache = Arc<Mutex<Option<Arc<dyn solascope_core::EmbeddingProvider>>>>;
type RerankerCache = Arc<Mutex<Option<Arc<dyn solascope_core::RerankProvider>>>>;

#[derive(Clone)]
pub struct Engine {
    config: Config,
    query_cache: Arc<Mutex<QueryCache>>,
    embedding: EmbeddingCache,
    reranker: RerankerCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Symbol,
    Lexical,
    Semantic,
}

pub fn classify_query(query: &str) -> QueryKind {
    if query.contains(['"', '\n']) || query.to_lowercase().contains("error") {
        QueryKind::Lexical
    } else if !query.trim().contains(char::is_whitespace) {
        QueryKind::Symbol
    } else {
        QueryKind::Semantic
    }
}

impl Engine {
    pub fn new(mut config: Config) -> Result<Self> {
        config.project.root = config.project.root.canonicalize()?;
        if config.project.index_dir.is_relative() {
            config.project.index_dir = config.project.root.join(&config.project.index_dir);
        }
        config.validate()?;
        Ok(Self {
            config,
            query_cache: Arc::new(Mutex::new(HashMap::new())),
            embedding: Arc::new(Mutex::new(None)),
            reranker: Arc::new(Mutex::new(None)),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn embedding_provider(&self) -> Result<Option<Arc<dyn solascope_core::EmbeddingProvider>>> {
        let mut cached = self
            .embedding
            .lock()
            .map_err(|_| Error::Storage("embedding cache lock poisoned".into()))?;
        if cached.is_none() {
            *cached = build_embedding(&self.config.embedding, &self.config.privacy)?;
        }
        Ok(cached.clone())
    }

    fn reranker_provider(&self) -> Result<Option<Arc<dyn solascope_core::RerankProvider>>> {
        let mut cached = self
            .reranker
            .lock()
            .map_err(|_| Error::Storage("reranker cache lock poisoned".into()))?;
        if cached.is_none() {
            *cached = build_reranker(&self.config.rerank, &self.config.privacy)?;
        }
        Ok(cached.clone())
    }

    fn store(&self, create: bool) -> Result<Store> {
        Store::open(
            &self.config.project.index_dir,
            &self.config.project.root,
            create,
        )
    }

    pub fn status(&self) -> Result<IndexStatus> {
        let store = self.store(false)?;
        store.snapshot(|store| self.checked_status(store))
    }

    fn checked_status(&self, store: &Store) -> Result<IndexStatus> {
        let status = store.status()?;
        if store.meta("chunking")?.as_deref() != Some(self.chunking_signature().as_str()) {
            return Err(Error::IndexOutdated(
                "stored chunks use different chunker rules or parser settings".into(),
            ));
        }
        Ok(status)
    }

    pub async fn index(&self, rebuild: bool) -> Result<IndexReport> {
        self.index_with_cancel(rebuild, CancellationToken::new())
            .await
    }

    pub async fn index_with_cancel(
        &self,
        rebuild: bool,
        cancel: CancellationToken,
    ) -> Result<IndexReport> {
        let lock = WriterLock::acquire(&self.config.project.index_dir)?;
        self.index_locked(rebuild, &lock, &cancel).await
    }

    async fn index_until_stable(
        &self,
        lock: &WriterLock,
        cancel: &CancellationToken,
        debounce: Duration,
    ) -> Result<IndexReport> {
        loop {
            match self.index_locked(false, lock, cancel).await {
                Ok(report) => return Ok(report),
                Err(Error::SourceChanged(_)) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(debounce) => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn index_locked(
        &self,
        rebuild: bool,
        lock: &WriterLock,
        cancel: &CancellationToken,
    ) -> Result<IndexReport> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let embedding = self.embedding_provider()?;
        let fingerprint = embedding.as_ref().map(|provider| provider.fingerprint());
        let mut store = self.store(true)?;
        let recovered = store.recover_jobs(lock)?;
        let previous = store.current_generation()?;
        let mut warnings = Vec::new();
        let mut rebuild = rebuild;
        if !rebuild
            && previous
                .as_ref()
                .is_some_and(|generation| generation.fingerprint != fingerprint)
        {
            if self.config.embedding.fingerprint_policy != "rebuild" {
                return Err(Error::ModelIncompatible(
                    "the embedding configuration changed".into(),
                ));
            }
            warnings.push(
                "the embedding configuration changed; rebuilding because \
                 embedding.fingerprint_policy is \"rebuild\""
                    .into(),
            );
            rebuild = true;
        }
        // Chunks produced under other chunking rules are never mixed with new ones.
        let chunking = self.chunking_signature();
        let rechunk =
            previous.is_some() && store.meta("chunking")?.as_deref() != Some(chunking.as_str());
        if rechunk && !rebuild {
            warnings.push(
                "chunking rules or parser settings changed; every file was re-chunked".into(),
            );
        }
        if recovered > 0 {
            warnings.push(format!(
                "recovered {recovered} interrupted indexing job(s); previous snapshot was preserved"
            ));
        }
        let reparse_all = rebuild || rechunk;
        let job = store.begin_job(lock)?;
        let started = Instant::now();
        let work = async {
            let config = self.config.clone();
            let scan = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::task::spawn_blocking(move || solascope_scanner::scan(&config)) => {
                    result.map_err(|error| Error::Storage(format!("scan worker: {error}")))??
                }
            };
            warnings.extend(scan.warnings);
            let files_total = scan.files.len();
            let old_hashes = store.file_hashes()?;
            let paths: HashSet<&str> = scan
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect();
            let removed: Vec<String> = old_hashes
                .keys()
                .filter(|path| !paths.contains(path.as_str()))
                .cloned()
                .collect();
            let changed: Vec<_> = scan
                .files
                .into_iter()
                .filter(|file| reparse_all || old_hashes.get(&file.relative_path) != Some(&file.hash))
                .collect();
            let files_updated = changed.len();
            let files_unchanged = files_total - files_updated;
            tracing::info!(
                files = files_total,
                changed = files_updated,
                deleted = removed.len(),
                "scan complete"
            );
            let id = project_id(&self.config.project.root);
            let options = self.parser_options();
            let jobs = self.config.index.max_concurrent_jobs;
            let updates: Vec<FileUpdate> = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::task::spawn_blocking(move || parse_files(changed, &id, &options, jobs)) => {
                    result.map_err(|error| Error::Parse(format!("parser worker: {error}")))??
                }
            };
            let skipped_ast = if self.config.parser.fallback_policy == "skip" {
                updates.iter().filter(|update| update.chunks.is_empty()
                    && !update.source.content.trim().is_empty()).count()
            } else { 0 };
            if skipped_ast > 0 {
                warnings.push(format!("skipped chunks for {skipped_ast} file(s) without a usable AST (fallback_policy=skip); file hashes retained for incremental checks"));
            }
            let mut vectors = Vec::new();
            let (mut cache_hits, mut relocated, mut embedded) = (0, 0, 0);
            if let (Some(provider), Some(fingerprint)) = (&embedding, &fingerprint) {
                let chunks: Vec<&Chunk> = updates.iter().flat_map(|update| &update.chunks).collect();
                let hashes: Vec<String> = chunks
                    .iter()
                    .map(|chunk| chunk.embedding_hash.clone())
                    .collect();
                // A rebuild recomputes vectors that were reused from another path.
                let cached = store.cached_vectors(fingerprint, &hashes, rebuild)?;
                let mut missing: BTreeMap<&str, &Chunk> = BTreeMap::new();
                for chunk in &chunks {
                    if !cached.contains_key(&chunk.embedding_hash) {
                        missing.entry(chunk.embedding_hash.as_str()).or_insert(chunk);
                    }
                }
                if !rebuild && !missing.is_empty() {
                    // Code that only moved keeps the vector from its old path.
                    let keys: Vec<String> = missing
                        .values()
                        .map(|chunk| chunk.relocation_key())
                        .collect();
                    let reusable = store.relocated_vectors(fingerprint, &keys)?;
                    missing.retain(|hash, chunk| {
                        let relocation_key = chunk.relocation_key();
                        let Some(vector) = reusable.get(&relocation_key) else {
                            return true;
                        };
                        vectors.push(CachedEmbedding {
                            input_hash: (*hash).to_owned(),
                            relocation_key,
                            vector: vector.clone(),
                            exact: false,
                        });
                        false
                    });
                    relocated = vectors.len();
                }
                embedded = missing.len();
                cache_hits = hashes.len() - embedded - relocated;
                if embedded > 0 {
                    let effective_batch_size = provider
                        .batch_size_hint()
                        .unwrap_or(self.config.embedding.batch_size)
                        .clamp(1, 128);
                    let group_inputs = (128 / effective_batch_size) * effective_batch_size;
                    tracing::info!(
                        inputs = embedded,
                        cache_hits,
                        relocated,
                        effective_batch_size,
                        concurrency = self.config.embedding.concurrency,
                        group_inputs,
                        "requesting embeddings"
                    );
                    let texts: Vec<String> = missing
                        .values()
                        .map(|chunk| chunk.embedding_text.clone())
                        .collect();
                    let mut generated = Vec::with_capacity(texts.len());
                    let embedding_started = Instant::now();
                    for batch in texts.chunks(group_inputs) {
                        let batch_started = Instant::now();
                        let output = tokio::select! {
                            _ = cancel.cancelled() => return Err(Error::Cancelled),
                            result = provider.embed(batch) => result.map_err(indexing_hint)?,
                        };
                        if output.len() != batch.len() {
                            return Err(Error::ModelIncompatible(
                                "embedding batch output count differs from input".into(),
                            ));
                        }
                        generated.extend(output);
                        let elapsed_seconds = embedding_started.elapsed().as_secs_f64();
                        let inputs_per_second = if elapsed_seconds > 0.0 {
                            generated.len() as f64 / elapsed_seconds
                        } else {
                            0.0
                        };
                        tracing::info!(
                            done = generated.len(),
                            total = embedded,
                            batch_elapsed_ms = milliseconds(batch_started.elapsed()),
                            inputs_per_second,
                            "embedding progress"
                        );
                    }
                    for ((hash, chunk), vector) in missing.iter().zip(generated) {
                        solascope_storage::validate_vector(&vector, fingerprint.dimensions)?;
                        vectors.push(CachedEmbedding {
                            input_hash: (*hash).to_owned(),
                            relocation_key: chunk.relocation_key(),
                            vector,
                            exact: true,
                        });
                    }
                }
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            for update in &updates {
                let current =
                    solascope_scanner::read_file(&self.config, &update.source.relative_path)
                        .map_err(|_| Error::SourceChanged(update.source.relative_path.clone()))?;
                if current.hash != update.source.hash {
                    return Err(Error::SourceChanged(update.source.relative_path.clone()));
                }
            }
            for path in &removed {
                if solascope_scanner::read_file(&self.config, path).is_ok() {
                    return Err(Error::SourceChanged(path.clone()));
                }
            }
            let generation = store.commit_index(
                lock,
                IndexCommit {
                    updates: &updates,
                    removed: &removed,
                    fingerprint: fingerprint.as_ref(),
                    vectors: &vectors,
                    force_generation: reparse_all,
                    chunking: &chunking,
                    job,
                    retention: self.config.index.snapshot_retention,
                },
            )?;
            let status = store.status()?;
            tracing::info!(
                version = generation.version,
                chunks = status.chunks,
                "index committed"
            );
            Ok(IndexReport {
                project_id: status.project_id,
                index_version: generation.version,
                files_total,
                files_updated,
                files_unchanged,
                files_deleted: removed.len(),
                chunks: status.chunks,
                embedded_chunks: embedded,
                cache_hits,
                relocated_embeddings: relocated,
                skipped_files: scan.skipped + skipped_ast,
                warnings: std::mem::take(&mut warnings),
                elapsed_ms: started.elapsed().as_millis(),
                rebuilt: rebuild,
            })
        }
        .await;
        if let Err(error) = &work {
            if let Err(record_error) = store.fail_job(lock, job, &error.to_string()) {
                tracing::warn!("could not record failed index job: {record_error}");
            }
        }
        work
    }

    fn parser_options(&self) -> ParserOptions {
        ParserOptions {
            enabled_languages: self.config.parser.enabled_languages.clone(),
            fallback: self.config.parser.fallback_policy == "text",
            max_chunk_tokens: self.config.parser.max_chunk_tokens,
            overlap_lines: self.config.parser.overlap_lines,
        }
    }

    /// Changes whenever the same source would be chunked differently, either
    /// by a new chunker or by different parser settings.
    fn chunking_signature(&self) -> String {
        let parser = &self.config.parser;
        let mut languages = parser.enabled_languages.clone();
        languages.sort();
        solascope_core::hash_text(&format!(
            "{}\0{}\0{}\0{}\0{}",
            solascope_parser::CHUNKER_VERSION,
            languages.join(","),
            parser.fallback_policy,
            parser.max_chunk_tokens,
            parser.overlap_lines
        ))
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        request.validate()?;
        let started = Instant::now();
        let mut embedding_time = Duration::ZERO;
        let preliminary = self.status()?;
        let mut warnings = Vec::new();
        let mut query_vector = None;
        let mut fingerprint_key = None;
        if request.mode != SearchMode::Lexical {
            if let Some(provider) = self.embedding_provider()? {
                let fingerprint = provider.fingerprint();
                if preliminary.embedding_fingerprint.as_ref() != Some(&fingerprint) {
                    return Err(Error::ModelIncompatible(
                        "query model differs from the active index".into(),
                    ));
                }
                let key = fingerprint.key();
                let cache_key =
                    solascope_core::hash_text(&format!("{key}\0{}", request.query.trim()));
                let cached = {
                    let cache = self
                        .query_cache
                        .lock()
                        .map_err(|_| Error::Storage("query cache lock poisoned".into()))?;
                    cache
                        .get(&cache_key)
                        .filter(|(time, _)| time.elapsed() < Duration::from_secs(300))
                        .map(|(_, vector)| vector.clone())
                };
                let vector = if let Some(vector) = cached {
                    Ok(vector)
                } else {
                    let embed_started = Instant::now();
                    let embedded = provider.embed_query(&request.query).await;
                    embedding_time = embed_started.elapsed();
                    match embedded {
                        Ok(vector) => {
                            solascope_storage::validate_vector(&vector, fingerprint.dimensions)?;
                            let mut cache = self
                                .query_cache
                                .lock()
                                .map_err(|_| Error::Storage("query cache lock poisoned".into()))?;
                            cache.retain(|_, (time, _)| time.elapsed() < Duration::from_secs(300));
                            if cache.len() >= 128 {
                                cache.clear();
                            }
                            cache.insert(cache_key, (Instant::now(), vector.clone()));
                            Ok(vector)
                        }
                        Err(error) => Err(error),
                    }
                };
                match vector {
                    Ok(vector) => {
                        query_vector = Some(vector);
                        fingerprint_key = Some(key);
                    }
                    Err(error)
                        if request.mode == SearchMode::Hybrid
                            && self.config.search.semantic_fail_open
                            && matches!(
                                error,
                                Error::ModelUnavailable(_) | Error::Authentication(_)
                            ) =>
                    {
                        warnings.push(format!(
                            "semantic recall unavailable; using lexical and symbols: {error}"
                        ));
                    }
                    Err(error) => return Err(error),
                }
            } else if request.mode == SearchMode::Semantic {
                return Err(Error::ModelUnavailable(
                    "embedding is disabled; configure a provider and rebuild the index".into(),
                ));
            } else {
                warnings.push(
                    "embedding is disabled; hybrid search uses lexical and symbol recall".into(),
                );
            }
        }
        let store = self.store(false)?;
        let (status, channels) = store.snapshot(|store| {
            let status = self.checked_status(store)?;
            let mut channels = Vec::new();
            if request.mode != SearchMode::Semantic {
                channels.push((
                    "lexical",
                    store.lexical_search(
                        &request.query,
                        self.config.search.lexical_top_k.max(request.limit),
                        request.path_filter.as_deref(),
                    )?,
                ));
                if request.mode == SearchMode::Hybrid {
                    channels.push((
                        "symbol",
                        store.symbol_search(
                            &SymbolRequest {
                                name: request.query.clone(),
                                kind: None,
                                path_filter: request.path_filter.clone(),
                                limit: self.config.search.symbol_top_k.min(MAX_RESULTS),
                            },
                            false,
                        )?,
                    ));
                }
            }
            if let (Some(vector), Some(key)) = (&query_vector, &fingerprint_key) {
                channels.push((
                    "vector",
                    store.vector_search(
                        vector,
                        key,
                        self.config.search.vector_top_k.max(request.limit),
                        request.path_filter.as_deref(),
                    )?,
                ));
            }
            Ok((status, channels))
        })?;
        drop(store);
        let symbol_query = classify_query(&request.query) == QueryKind::Symbol
            && request.mode != SearchMode::Semantic;
        let supporting_code_wanted = mentions_supporting_code(&request.query)
            || request
                .path_filter
                .as_deref()
                .is_some_and(mentions_supporting_code);
        let mut candidates = fuse(
            channels,
            self.config.search.fusion_k,
            symbol_query,
            supporting_code_wanted,
        );
        candidates.sort_by(candidate_order);
        // Only candidates that can still be returned or reranked need their
        // source re-read; the margin absorbs duplicates and filtered entries.
        candidates.truncate(
            (request.limit * 3).max(self.config.search.rerank_candidates + request.limit),
        );
        self.validate_candidates(&mut candidates, &mut warnings);
        let mut rerank_time = Duration::ZERO;
        if request.rerank && !candidates.is_empty() && self.config.rerank.provider != "disabled" {
            let rerank_started = Instant::now();
            let reranked = self.rerank(&request.query, &candidates).await;
            rerank_time = rerank_started.elapsed();
            match reranked {
                Ok(mut scores) => {
                    scores.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
                    let positions: HashMap<String, (usize, f64)> = scores
                        .into_iter()
                        .enumerate()
                        .map(|(rank, item)| (item.document_id, (rank, item.score)))
                        .collect();
                    candidates.sort_by(|a, b| {
                        b.exact.cmp(&a.exact).then_with(|| {
                            match (
                                positions.get(&a.ranked.chunk.id),
                                positions.get(&b.ranked.chunk.id),
                            ) {
                                (Some((a, _)), Some((b, _))) => a.cmp(b),
                                (Some(_), None) => std::cmp::Ordering::Less,
                                (None, Some(_)) => std::cmp::Ordering::Greater,
                                (None, None) => candidate_order(a, b),
                            }
                        })
                    });
                    for candidate in &mut candidates {
                        if let Some((_, score)) = positions.get(&candidate.ranked.chunk.id) {
                            candidate.score = *score;
                            candidate.sources.insert("rerank".into());
                        }
                    }
                }
                Err(error) if self.config.rerank.fail_open => warnings.push(format!(
                    "reranker unavailable; preserving fused ranking: {error}"
                )),
                Err(error) => return Err(error),
            }
            self.validate_candidates(&mut candidates, &mut warnings);
        }
        let total = started.elapsed();
        Ok(make_response(
            request.query,
            request.mode,
            status.index_version,
            candidates,
            request.limit,
            warnings,
            SearchTimings {
                embedding_ms: milliseconds(embedding_time),
                retrieval_ms: milliseconds(total.saturating_sub(embedding_time + rerank_time)),
                rerank_ms: milliseconds(rerank_time),
                total_ms: milliseconds(total),
            },
        ))
    }

    /// Scores the leading candidates that still match their source. Scores come
    /// back empty when no candidate is eligible.
    async fn rerank(&self, query: &str, candidates: &[Candidate]) -> Result<Vec<RerankScore>> {
        let provider = self
            .reranker_provider()?
            .ok_or_else(|| Error::ModelUnavailable("reranker is disabled".into()))?;
        // What the provider can take bounds what the configuration asks for.
        let limits = provider.limits();
        let max_chars = self
            .config
            .rerank
            .max_input_chars
            .min(limits.max_input_chars);
        let documents: Vec<RerankDocument> = candidates
            .iter()
            .filter(|candidate| !candidate.stale)
            .take(
                self.config
                    .search
                    .rerank_candidates
                    .min(limits.max_documents),
            )
            .map(|candidate| RerankDocument {
                id: candidate.ranked.chunk.id.clone(),
                text: candidate
                    .ranked
                    .chunk
                    .embedding_text
                    .chars()
                    .take(max_chars)
                    .collect(),
            })
            .collect();
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let scores = provider.rerank(query, &documents).await?;
        if scores.is_empty() {
            return Err(Error::ModelUnavailable(
                "reranker returned no scores".into(),
            ));
        }
        let mut seen = HashSet::new();
        for score in &scores {
            if !score.score.is_finite()
                || score.index >= documents.len()
                || documents[score.index].id != score.document_id
                || !seen.insert(score.index)
            {
                return Err(Error::ModelIncompatible(
                    "invalid reranker candidate mapping".into(),
                ));
            }
        }
        Ok(scores)
    }

    pub fn find_symbol(&self, request: SymbolRequest) -> Result<SearchResponse> {
        request.validate()?;
        let started = Instant::now();
        let store = self.store(false)?;
        let (status, ranked) = store.snapshot(|store| {
            Ok((
                self.checked_status(store)?,
                store.symbol_search(&request, true)?,
            ))
        })?;
        let mut warnings = Vec::new();
        let mut candidates: Vec<Candidate> = ranked
            .into_iter()
            .map(|ranked| Candidate {
                score: ranked.score,
                // Near misses are labelled so a typo correction is visible.
                sources: BTreeSet::from([if ranked.score < 25.0 {
                    "fuzzy".into()
                } else {
                    "symbol".into()
                }]),
                ranked,
                exact: false,
                stale: false,
            })
            .collect();
        self.validate_candidates(&mut candidates, &mut warnings);
        let total = milliseconds(started.elapsed());
        Ok(make_response(
            request.name,
            SearchMode::Lexical,
            status.index_version,
            candidates,
            request.limit,
            warnings,
            SearchTimings {
                retrieval_ms: total,
                total_ms: total,
                ..Default::default()
            },
        ))
    }

    fn validate_candidates(&self, candidates: &mut Vec<Candidate>, warnings: &mut Vec<String>) {
        enum Visibility {
            Current(String),
            Changed,
            Missing,
            Blocked,
        }
        let mut visibility = HashMap::new();
        for candidate in candidates.iter() {
            visibility
                .entry(candidate.ranked.chunk.relative_path.clone())
                .or_insert_with(|| {
                    match solascope_scanner::read_file(
                        &self.config,
                        &candidate.ranked.chunk.relative_path,
                    ) {
                        Ok(file) => Visibility::Current(file.hash),
                        // A normal save during reading is not a privacy failure.
                        // Keep the indexed snapshot and make its staleness explicit.
                        Err(Error::SourceChanged(_)) => Visibility::Changed,
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                            Visibility::Missing
                        }
                        Err(_) => Visibility::Blocked,
                    }
                });
        }
        let before = candidates.len();
        candidates.retain_mut(|candidate| {
            match visibility.get(&candidate.ranked.chunk.relative_path) {
                Some(Visibility::Current(hash)) => {
                    candidate.stale = *hash != candidate.ranked.file_hash;
                    true
                }
                Some(Visibility::Changed) => {
                    candidate.stale = true;
                    true
                }
                Some(Visibility::Missing) => {
                    candidate.stale = true;
                    candidate.ranked.chunk.content.clear();
                    true
                }
                _ => false,
            }
        });
        if before != candidates.len() {
            warnings.push(format!(
                "{} indexed candidate(s) excluded by current source access/privacy policy",
                before - candidates.len()
            ));
        }
        let stale = candidates
            .iter()
            .filter(|candidate| candidate.stale)
            .count();
        if stale > 0 {
            warnings.push(format!(
                "{stale} candidate(s) are stale; run `solascope index` to refresh positions"
            ));
        }
    }

    pub fn context(&self, request: ContextRequest) -> Result<ContextResponse> {
        solascope_core::validate_relative_path(&request.path)?;
        if request.before > MAX_CONTEXT_LINES || request.after > MAX_CONTEXT_LINES {
            return Err(Error::InvalidInput(
                "context radius exceeds 200 lines".into(),
            ));
        }
        if request.line.is_some() && (request.start_line.is_some() || request.end_line.is_some()) {
            return Err(Error::InvalidInput(
                "use either a center line or an explicit range".into(),
            ));
        }
        if request.line.is_some() && request.before + request.after + 1 > MAX_CONTEXT_LINES {
            return Err(Error::InvalidInput(
                "context radius exceeds 200 lines".into(),
            ));
        }
        let source = solascope_scanner::read_file(&self.config, &request.path)?;
        let lines: Vec<&str> = source.content.lines().collect();
        if lines.is_empty() {
            return Err(Error::InvalidInput("source file is empty".into()));
        }
        let (start, end) = if let Some(line) = request.line {
            if line == 0 || line > lines.len() {
                return Err(Error::InvalidInput(
                    "center line is outside the source file".into(),
                ));
            }
            (
                line.saturating_sub(request.before).max(1),
                line.saturating_add(request.after).min(lines.len()),
            )
        } else {
            let start = request.start_line.unwrap_or(1);
            (
                start,
                request
                    .end_line
                    .unwrap_or_else(|| start.saturating_add(20).min(lines.len())),
            )
        };
        if start == 0 || end < start || end > lines.len() || end - start + 1 > MAX_CONTEXT_LINES {
            return Err(Error::InvalidInput(
                "context range must be valid and at most 200 lines".into(),
            ));
        }
        let selected = &lines[start - 1..end];
        if selected.iter().map(|line| line.len() + 1).sum::<usize>() > MAX_CONTEXT_BYTES {
            return Err(Error::InvalidInput("context exceeds 64 KiB".into()));
        }
        Ok(ContextResponse {
            relative_path: source.relative_path,
            start_line: start,
            end_line: end,
            lines: selected
                .iter()
                .enumerate()
                .map(|(offset, text)| ContextLine {
                    line: start + offset,
                    text: (*text).into(),
                })
                .collect(),
        })
    }

    pub fn prune_cache(&self) -> Result<usize> {
        let lock = WriterLock::acquire(&self.config.project.index_dir)?;
        self.store(true)?.prune_unused_cache(&lock)
    }

    pub async fn doctor(&self, check_models: bool) -> Result<DoctorReport> {
        let mut checks = vec![DiagnosticCheck {
            name: "configuration".into(),
            status: "ok".into(),
            detail: "configuration and privacy boundaries are valid".into(),
        }];
        let database = self.store(false).and_then(|store| {
            store.snapshot(|store| {
                store.integrity_check()?;
                self.checked_status(store)
            })
        });
        let (status, detail) = match database {
            Ok(status) => (
                "ok",
                format!(
                    "SQLite/FTS consistent, schema {}, generation {}, {} files, {} chunks",
                    solascope_storage::SCHEMA_VERSION,
                    status.index_version,
                    status.files,
                    status.chunks
                ),
            ),
            Err(Error::IndexMissing) => ("warning", "no index yet; run `solascope index`".into()),
            Err(error @ Error::IndexOutdated(_)) => ("warning", error.to_string()),
            Err(error) => ("error", error.to_string()),
        };
        checks.push(DiagnosticCheck {
            name: "database".into(),
            status: status.into(),
            detail,
        });
        checks.extend(self.model_checks(check_models).await);
        Ok(DoctorReport {
            healthy: checks.iter().all(|check| check.status != "error"),
            checks,
        })
    }

    /// Sends one request to each configured model and reports whether it
    /// answered. The request carries a fixed sentence, never project source.
    pub async fn probe_models(&self) -> Vec<DiagnosticCheck> {
        self.model_checks(true).await
    }

    async fn model_checks(&self, probe: bool) -> Vec<DiagnosticCheck> {
        const PROBE: &str = "SolaScope capability probe";
        const UNPROBED: &str = "configured; use doctor --models to probe";
        let embedding = async {
            let Some(provider) = self.embedding_provider()? else {
                return Ok("disabled; no inference service required".to_owned());
            };
            if !probe {
                return Ok(UNPROBED.to_owned());
            }
            let vectors = provider.embed(&[PROBE.into()]).await?;
            Ok::<_, Error>(format!(
                "provider answered with {} dimensions",
                vectors.first().map_or(0, Vec::len)
            ))
        }
        .await;
        let reranker = async {
            let Some(provider) = self.reranker_provider()? else {
                return Ok("disabled; fused ranking remains available".to_owned());
            };
            if !probe {
                return Ok(UNPROBED.to_owned());
            }
            let document = RerankDocument {
                id: "probe".into(),
                text: PROBE.into(),
            };
            provider.rerank("capability probe", &[document]).await?;
            Ok::<_, Error>(format!("{} answered", provider.name()))
        }
        .await;
        [("embedding", embedding), ("reranker", reranker)]
            .into_iter()
            .map(|(name, result)| match result {
                Ok(detail) => DiagnosticCheck {
                    name: name.into(),
                    status: "ok".into(),
                    detail,
                },
                Err(error) => DiagnosticCheck {
                    name: name.into(),
                    status: "error".into(),
                    detail: error.to_string(),
                },
            })
            .collect()
    }

    pub async fn watch(&self, cancel: CancellationToken) -> Result<()> {
        let lock = WriterLock::acquire(&self.config.project.index_dir)?;
        let (_watcher, mut events) = solascope_scanner::watch(&self.config.project.root)?;
        let debounce = Duration::from_millis(self.config.index.debounce_ms);
        match self.index_until_stable(&lock, &cancel, debounce).await {
            Ok(_) => {}
            Err(Error::Cancelled) if cancel.is_cancelled() => return Ok(()),
            Err(error) => return Err(error),
        }
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                event = events.recv() => event.ok_or_else(|| Error::Storage("file watcher stopped".into()))?,
            }.map_err(|error| Error::Storage(format!("file watcher: {error}")))?;
            if !solascope_scanner::event_is_relevant(&self.config, &event) {
                continue;
            }
            let sleep = tokio::time::sleep(debounce);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = &mut sleep => break,
                    event = events.recv() => {
                        let event = event.ok_or_else(|| Error::Storage("file watcher stopped".into()))?
                            .map_err(|error| Error::Storage(format!("file watcher: {error}")))?;
                        if solascope_scanner::event_is_relevant(&self.config, &event) {
                            sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                        }
                    }
                }
            }
            match self.index_until_stable(&lock, &cancel, debounce).await {
                Ok(report) => tracing::info!(
                    updated = report.files_updated,
                    deleted = report.files_deleted,
                    version = report.index_version,
                    "incremental index committed"
                ),
                Err(Error::Cancelled) if cancel.is_cancelled() => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

struct Candidate {
    ranked: RankedChunk,
    score: f64,
    sources: BTreeSet<String>,
    exact: bool,
    stale: bool,
}

fn candidate_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.exact
        .cmp(&a.exact)
        .then(b.score.total_cmp(&a.score))
        .then(a.ranked.chunk.id.cmp(&b.ranked.chunk.id))
}

/// Weights on the fused score that prefer named definitions in product code.
/// With the default `fusion_k`, neighbouring ranks differ by under two percent,
/// so these move a candidate a few positions instead of hiding it.
const ANONYMOUS_TEXT_WEIGHT: f64 = 0.97;
const SUPPORTING_PATH_WEIGHT: f64 = 0.94;
const SUPPORTING_WORDS: [&str; 5] = ["test", "spec", "bench", "example", "fixture"];

/// Whether a query or path filter asks for tests, examples or benchmarks.
fn mentions_supporting_code(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    SUPPORTING_WORDS.iter().any(|word| text.contains(word))
}

/// Code that exercises the product rather than being it.
fn is_supporting_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or_default();
    parts.any(|directory| {
        matches!(
            directory,
            "test"
                | "tests"
                | "__tests__"
                | "testdata"
                | "spec"
                | "specs"
                | "bench"
                | "benches"
                | "benchmarks"
                | "example"
                | "examples"
                | "fixtures"
        )
    }) || name.starts_with("test_")
        || name == "conftest.py"
        || ["_test.", "_tests.", ".test.", ".spec.", "_spec."]
            .iter()
            .any(|marker| name.contains(marker))
}

fn fuse(
    channels: Vec<(&str, Vec<RankedChunk>)>,
    fusion_k: f64,
    symbol_query: bool,
    supporting_code_wanted: bool,
) -> Vec<Candidate> {
    let mut candidates: HashMap<String, Candidate> = HashMap::new();
    for (source, ranked) in channels {
        for (rank, item) in ranked.into_iter().enumerate() {
            let exact = symbol_query && source == "symbol" && item.score >= 100.0;
            let id = item.chunk.id.clone();
            let candidate = candidates.entry(id).or_insert_with(|| Candidate {
                ranked: item,
                score: 0.0,
                sources: BTreeSet::new(),
                exact: false,
                stale: false,
            });
            candidate.score += 1.0 / (fusion_k + rank as f64 + 1.0);
            candidate.sources.insert(source.into());
            candidate.exact |= exact;
        }
    }
    candidates
        .into_values()
        .map(|mut candidate| {
            let chunk = &candidate.ranked.chunk;
            if chunk.symbol.is_none() {
                candidate.score *= ANONYMOUS_TEXT_WEIGHT;
            }
            if !supporting_code_wanted && is_supporting_path(&chunk.relative_path) {
                candidate.score *= SUPPORTING_PATH_WEIGHT;
            }
            candidate
        })
        .collect()
}

fn milliseconds(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// Tells the user how to proceed when the embedding service fails mid-index.
fn indexing_hint(error: Error) -> Error {
    match error {
        Error::ModelUnavailable(message) => Error::ModelUnavailable(format!(
            "{message}; the existing index is unchanged. Start the embedding service and retry, \
             or set embedding.provider = \"disabled\" and run `solascope index --rebuild` to \
             search by keywords and symbols only"
        )),
        other => other,
    }
}

fn parse_files(
    files: Vec<SourceFile>,
    project_id: &str,
    options: &ParserOptions,
    jobs: usize,
) -> Result<Vec<FileUpdate>> {
    use rayon::prelude::*;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .map_err(|error| Error::Parse(format!("parser pool: {error}")))?;
    pool.install(|| {
        files
            .into_par_iter()
            .map(|source| {
                let chunks = match solascope_parser::parse_file(
                    project_id,
                    &source.relative_path,
                    &source.content,
                    options,
                ) {
                    Ok(chunks) => chunks,
                    // Retain the file hash but remove any previous chunks. A
                    // later valid revision is then parsed incrementally.
                    Err(Error::Parse(_)) if !options.fallback => Vec::new(),
                    Err(error) => return Err(error),
                };
                Ok(FileUpdate { source, chunks })
            })
            .collect()
    })
}

fn make_response(
    query: String,
    mode: SearchMode,
    version: u64,
    candidates: Vec<Candidate>,
    limit: usize,
    mut warnings: Vec<String>,
    timings: SearchTimings,
) -> SearchResponse {
    let mut seen_warnings = HashSet::new();
    warnings.retain(|warning| seen_warnings.insert(warning.clone()));
    let mut seen = HashSet::new();
    let results = candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.ranked.chunk.entity_id.clone()))
        .take(limit)
        .enumerate()
        .map(|(index, candidate)| {
            let chunk = candidate.ranked.chunk;
            SearchResult {
                project_id: chunk.project_id,
                relative_path: chunk.relative_path,
                symbol: chunk.symbol,
                kind: chunk.kind,
                language: chunk.language,
                signature: chunk.signature,
                parent_symbol: chunk.parent_symbol,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                snippet: truncate_text(&chunk.content, 1600),
                score: candidate.score,
                rank: index + 1,
                match_sources: candidate.sources.into_iter().collect(),
                index_version: version,
                stale: candidate.stale,
            }
        })
        .collect();
    SearchResponse {
        query,
        mode,
        index_version: version,
        results,
        warnings,
        timings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_symbols_errors_and_natural_language() {
        assert_eq!(classify_query("Auth::login"), QueryKind::Symbol);
        assert_eq!(
            classify_query("error connecting to server"),
            QueryKind::Lexical
        );
        assert_eq!(
            classify_query("where are users created"),
            QueryKind::Semantic
        );
    }

    #[test]
    fn engine_clones_reuse_model_adapters_without_probing() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default_for_root(root.path()).unwrap();
        config.embedding.provider = "ollama".into();
        config.rerank.provider = "remote".into();
        config.rerank.model = "test".into();
        config.rerank.endpoint = "http://127.0.0.1:1".into();
        config.privacy.remote_opt_in = true;
        let engine = Engine::new(config).unwrap();
        let cloned = engine.clone();
        assert!(Arc::ptr_eq(
            &engine.embedding_provider().unwrap().unwrap(),
            &cloned.embedding_provider().unwrap().unwrap(),
        ));
        assert!(Arc::ptr_eq(
            &engine.reranker_provider().unwrap().unwrap(),
            &cloned.reranker_provider().unwrap().unwrap(),
        ));
    }

    #[tokio::test]
    async fn rerank_revalidates_source_positions_and_visibility_after_http_wait() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        for hide_source in [false, true] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("lib.rs"), "fn greet() {}\n").unwrap();
            let server = MockServer::start().await;
            let mut config = Config::default_for_root(root.path()).unwrap();
            config.privacy.remote_opt_in = true;
            config.rerank.provider = "remote".into();
            config.rerank.model = "test".into();
            config.rerank.endpoint = server.uri();
            let engine = Engine::new(config.clone()).unwrap();
            let mut mock_config = config.rerank;
            mock_config.api_key_env.clear();
            *engine.reranker.lock().unwrap() =
                build_reranker(&mock_config, &config.privacy).unwrap();
            engine.index(false).await.unwrap();
            let directory = root.path().to_path_buf();
            Mock::given(method("POST"))
                .and(path("/rerank"))
                .respond_with(move |_: &wiremock::Request| {
                    if hide_source {
                        std::fs::write(directory.join(".solascopeignore"), "lib.rs\n").unwrap();
                    } else {
                        std::fs::write(
                            directory.join("lib.rs"),
                            format!("{}fn greet() {{}}\n", "\n".repeat(40)),
                        )
                        .unwrap();
                    }
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "results": [{"index": 0, "relevance_score": 1.0}]
                    }))
                })
                .mount(&server)
                .await;
            let response = engine
                .search(SearchRequest {
                    query: "greet".into(),
                    mode: SearchMode::Lexical,
                    rerank: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            if hide_source {
                assert!(response.results.is_empty());
            } else {
                assert_eq!(response.results[0].start_line, 1);
                assert!(response.results[0].stale);
            }
        }
    }
}
