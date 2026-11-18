use globset::{Glob, GlobMatcher};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use solascope_core::{
    project_id, validate_relative_path, Chunk, Error, IndexGeneration, IndexStatus, LexicalIndex,
    MetadataStore, ModelFingerprint, RankedChunk, Result, SourceFile, SymbolRequest, VectorIndex,
};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 1;
pub const DATABASE_NAME: &str = "index.sqlite3";

fn storage_error(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

fn sqlite_integer<T: TryInto<i64>>(value: T) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| Error::Storage("integer exceeds SQLite's signed range".into()))
}

pub fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct WriterLock {
    file: File,
    index_dir: PathBuf,
}

impl WriterLock {
    pub fn acquire(index_dir: &Path) -> Result<Self> {
        fs::create_dir_all(index_dir)?;
        let index_dir = index_dir.canonicalize()?;
        let path = index_dir.join("writer.lock");
        if path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(Error::Privacy(
                "index lock cannot be a symbolic link".into(),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Error::WriterBusy
            } else {
                Error::Io(error)
            }
        })?;
        Ok(Self { file, index_dir })
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Clone)]
pub struct FileUpdate {
    pub source: SourceFile,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone)]
pub struct CachedEmbedding {
    pub input_hash: String,
    pub vector: Vec<f32>,
}

pub struct Store {
    connection: Connection,
    root: PathBuf,
    index_dir: PathBuf,
    project_id: String,
}

impl Store {
    pub fn open(index_dir: &Path, root: &Path, create: bool) -> Result<Self> {
        let root = root.canonicalize()?;
        if create {
            fs::create_dir_all(index_dir)?;
        }
        let db_path = index_dir.join(DATABASE_NAME);
        if !create && !db_path.is_file() {
            return Err(Error::IndexMissing);
        }
        if db_path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(Error::Privacy(
                "index database cannot be a symbolic link".into(),
            ));
        }
        let flags = if create {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let mut connection = Connection::open_with_flags(&db_path, flags).map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_error)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(storage_error)?;
        if version == 0 && create {
            let tables: i64 = connection.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [], |row| row.get(0),
            ).map_err(storage_error)?;
            if tables != 0 {
                return Err(Error::Storage(
                    "refusing to migrate an unrelated database".into(),
                ));
            }
            let tx = connection.transaction().map_err(storage_error)?;
            tx.execute_batch(include_str!("schema.sql"))
                .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
        } else if version != SCHEMA_VERSION {
            return Err(Error::Storage(format!(
                "unsupported schema {version}; this binary requires {SCHEMA_VERSION}"
            )));
        }
        if create {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(storage_error)?;
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(storage_error)?;
        }
        let id = project_id(&root);
        let recorded: Option<(String, String)> = connection
            .query_row("SELECT id, root FROM projects LIMIT 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .map_err(storage_error)?;
        match recorded {
            Some((existing, existing_root))
                if existing != id || existing_root != root.to_string_lossy() =>
            {
                return Err(Error::Storage(
                    "this index belongs to another project root".into(),
                ));
            }
            None if create => {
                connection
                    .execute(
                        "INSERT INTO projects(id, root, created_at) VALUES (?1, ?2, ?3)",
                        params![id, root.to_string_lossy(), sqlite_integer(timestamp())?],
                    )
                    .map_err(storage_error)?;
            }
            None => return Err(Error::IndexMissing),
            _ => {}
        }
        Ok(Self {
            connection,
            root,
            index_dir: index_dir.canonicalize()?,
            project_id: id,
        })
    }

    fn verify_lock(&self, lock: &WriterLock) -> Result<()> {
        if lock.index_dir != self.index_dir {
            return Err(Error::Storage(
                "writer lock belongs to another index".into(),
            ));
        }
        Ok(())
    }

    pub fn snapshot<T>(&self, read: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(storage_error)?;
        let output = read(self)?;
        tx.commit().map_err(storage_error)?;
        Ok(output)
    }

    pub fn current_generation(&self) -> Result<Option<IndexGeneration>> {
        let row: Option<(i64, Option<String>, i64)> = self.connection
            .query_row(
                "SELECT version, fingerprint, created_at FROM index_generations ORDER BY version DESC LIMIT 1",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(storage_error)?;
        row.map(|(version, fingerprint, created_at)| {
            Ok(IndexGeneration {
                version: u64::try_from(version).map_err(storage_error)?,
                fingerprint: fingerprint
                    .map(|value| serde_json::from_str(&value).map_err(storage_error))
                    .transpose()?,
                created_at: u64::try_from(created_at).map_err(storage_error)?,
            })
        })
        .transpose()
    }

    pub fn file_hashes(&self) -> Result<BTreeMap<String, String>> {
        let mut statement = self
            .connection
            .prepare("SELECT path, hash FROM files ORDER BY path")
            .map_err(storage_error)?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(storage_error)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(storage_error)
    }

    pub fn cached_vectors(
        &self,
        fingerprint: &ModelFingerprint,
        hashes: &[String],
    ) -> Result<HashMap<String, Vec<f32>>> {
        let mut result = HashMap::new();
        let mut statement = self.connection.prepare(
            "SELECT dimensions, vector FROM embedding_cache WHERE fingerprint=?1 AND input_hash=?2"
        ).map_err(storage_error)?;
        let key = fingerprint.key();
        for hash in hashes {
            if result.contains_key(hash) {
                continue;
            }
            let value: Option<(i64, Vec<u8>)> = statement
                .query_row(params![key, hash], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
                .map_err(storage_error)?;
            if let Some((dimensions, blob)) = value {
                let dimensions = usize::try_from(dimensions).map_err(storage_error)?;
                if dimensions != fingerprint.dimensions {
                    return Err(Error::ModelIncompatible(
                        "cached dimensions differ from the configured model".into(),
                    ));
                }
                let vector = decode_vector(&blob, dimensions)?;
                validate_vector(&vector, dimensions)?;
                result.insert(hash.clone(), vector);
            }
        }
        Ok(result)
    }

    pub fn recover_jobs(&self, lock: &WriterLock) -> Result<usize> {
        self.verify_lock(lock)?;
        self.connection.execute(
            "UPDATE index_jobs SET status='interrupted', finished_at=?1, error='previous writer interrupted' WHERE status='running'",
            [sqlite_integer(timestamp())?],
        ).map_err(storage_error)
    }

    pub fn begin_job(&self, lock: &WriterLock) -> Result<i64> {
        self.verify_lock(lock)?;
        self.connection
            .execute(
                "INSERT INTO index_jobs(started_at, status) VALUES (?1, 'running')",
                [sqlite_integer(timestamp())?],
            )
            .map_err(storage_error)?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn fail_job(&self, lock: &WriterLock, job: i64, error: &str) -> Result<()> {
        self.verify_lock(lock)?;
        self.connection
            .execute(
                "UPDATE index_jobs SET status='failed', finished_at=?1, error=?2 WHERE id=?3",
                params![
                    sqlite_integer(timestamp())?,
                    solascope_core::truncate_text(error, 300),
                    job
                ],
            )
            .map_err(storage_error)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn commit_index(
        &mut self,
        lock: &WriterLock,
        updates: &[FileUpdate],
        removed: &[String],
        fingerprint: Option<&ModelFingerprint>,
        vectors: &[CachedEmbedding],
        force_generation: bool,
        job: i64,
        retention: usize,
    ) -> Result<IndexGeneration> {
        self.verify_lock(lock)?;
        let previous = self.current_generation()?;
        let changed = force_generation
            || !updates.is_empty()
            || !removed.is_empty()
            || previous
                .as_ref()
                .is_none_or(|generation| generation.fingerprint.as_ref() != fingerprint);
        let version = if changed {
            previous
                .as_ref()
                .map_or(0, |generation| generation.version)
                .checked_add(1)
                .ok_or_else(|| Error::Storage("generation overflow".into()))?
        } else {
            previous.as_ref().map_or(1, |generation| generation.version)
        };
        let now = timestamp();
        let tx = self.connection.transaction().map_err(storage_error)?;
        if let Some(fingerprint) = fingerprint {
            let key = fingerprint.key();
            for item in vectors {
                validate_vector(&item.vector, fingerprint.dimensions)?;
                let blob = encode_vector(&item.vector);
                tx.execute(
                    "INSERT INTO embedding_cache(fingerprint, input_hash, dimensions, vector) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                    params![key, item.input_hash, sqlite_integer(fingerprint.dimensions)?, blob],
                ).map_err(storage_error)?;
            }
        } else if !vectors.is_empty() {
            return Err(Error::Storage(
                "vectors require an embedding fingerprint".into(),
            ));
        }
        for path in removed {
            validate_relative_path(path)?;
            tx.execute("DELETE FROM files WHERE path=?1", [path])
                .map_err(storage_error)?;
        }
        for update in updates {
            validate_relative_path(&update.source.relative_path)?;
            tx.execute(
                "DELETE FROM chunks WHERE path=?1",
                [&update.source.relative_path],
            )
            .map_err(storage_error)?;
            let language = update
                .chunks
                .first()
                .map_or("text", |chunk| chunk.language.as_str());
            tx.execute(
                "INSERT INTO files(path, hash, language) VALUES (?1, ?2, ?3) ON CONFLICT(path) DO UPDATE SET hash=excluded.hash, language=excluded.language",
                params![update.source.relative_path, update.source.hash, language],
            ).map_err(storage_error)?;
            for chunk in &update.chunks {
                if chunk.project_id != self.project_id
                    || chunk.relative_path != update.source.relative_path
                {
                    return Err(Error::Storage("chunk/source identity mismatch".into()));
                }
                tx.prepare_cached(
                    "INSERT INTO chunks(id, path, symbol, kind, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(storage_error)?
                .execute(params![
                    chunk.id,
                    chunk.relative_path,
                    chunk.symbol,
                    chunk.kind,
                    serde_json::to_string(chunk).map_err(storage_error)?
                ])
                .map_err(storage_error)?;
                if let Some(fingerprint) = fingerprint {
                    tx.prepare_cached(
                        "INSERT INTO chunk_embeddings(chunk_id, fingerprint, input_hash) VALUES (?1, ?2, ?3)",
                    )
                    .map_err(storage_error)?
                    .execute(
                        params![chunk.id, fingerprint.key(), chunk.embedding_hash],
                    ).map_err(storage_error)?;
                }
            }
        }
        if changed {
            let json = fingerprint
                .map(serde_json::to_string)
                .transpose()
                .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO index_generations(version, fingerprint, created_at) VALUES (?1, ?2, ?3)",
                params![sqlite_integer(version)?, json, sqlite_integer(now)?],
            ).map_err(storage_error)?;
            let cutoff = version.saturating_sub(retention.max(1) as u64);
            tx.execute(
                "DELETE FROM index_generations WHERE version<=?1",
                [sqlite_integer(cutoff)?],
            )
            .map_err(storage_error)?;
        }
        tx.execute(
            "UPDATE index_jobs SET status='done', finished_at=?1 WHERE id=?2",
            params![sqlite_integer(now)?, job],
        )
        .map_err(storage_error)?;
        tx.execute("DELETE FROM index_jobs WHERE id NOT IN (SELECT id FROM index_jobs ORDER BY id DESC LIMIT 64)", []).map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
        if changed {
            Ok(IndexGeneration {
                version,
                fingerprint: fingerprint.cloned(),
                created_at: now,
            })
        } else {
            previous.ok_or(Error::IndexMissing)
        }
    }

    pub fn prune_unused_cache(&self, lock: &WriterLock) -> Result<usize> {
        self.verify_lock(lock)?;
        self.connection.execute(
            "DELETE FROM embedding_cache WHERE NOT EXISTS (SELECT 1 FROM chunk_embeddings WHERE chunk_embeddings.fingerprint=embedding_cache.fingerprint AND chunk_embeddings.input_hash=embedding_cache.input_hash)",
            [],
        ).map_err(storage_error)
    }

    pub fn integrity_check(&self) -> Result<()> {
        let integrity: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(storage_error)?;
        if integrity != "ok" {
            return Err(Error::Storage(format!("SQLite integrity: {integrity}")));
        }
        let violation: Option<String> = self
            .connection
            .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
            .optional()
            .map_err(storage_error)?;
        if let Some(table) = violation {
            return Err(Error::Storage(format!("foreign key violation in {table}")));
        }
        let counts: (i64, i64) = self
            .connection
            .query_row(
                "SELECT (SELECT count(*) FROM chunks), (SELECT count(*) FROM chunk_fts)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage_error)?;
        if counts.0 != counts.1 {
            return Err(Error::Storage(
                "FTS/chunk count mismatch; rebuild the index".into(),
            ));
        }
        Ok(())
    }

    /// Loads the full rows for already ranked `(seq, score)` hits, preserving
    /// their order and stopping once `limit` chunks pass the path filter.
    fn load_ranked(
        &self,
        hits: Vec<(i64, f64)>,
        filter: &PathFilter,
        limit: usize,
    ) -> Result<Vec<RankedChunk>> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT c.path, c.data, f.hash FROM chunks c JOIN files f ON f.path=c.path WHERE c.seq=?1",
            )
            .map_err(storage_error)?;
        let mut result = Vec::with_capacity(limit.min(hits.len()));
        for (seq, score) in hits {
            if result.len() >= limit {
                break;
            }
            let row: Option<(String, String, String)> = statement
                .query_row([seq], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .optional()
                .map_err(storage_error)?;
            let Some((path, data, file_hash)) = row else {
                continue;
            };
            if !filter.matches(&path) {
                continue;
            }
            result.push(RankedChunk {
                chunk: serde_json::from_str(&data).map_err(storage_error)?,
                file_hash,
                score,
            });
        }
        Ok(result)
    }
}

impl MetadataStore for Store {
    fn status(&self) -> Result<IndexStatus> {
        let generation = self.current_generation()?.ok_or(Error::IndexMissing)?;
        let count = |table: &str| -> Result<usize> {
            let value: i64 = self
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(storage_error)?;
            usize::try_from(value).map_err(storage_error)
        };
        let pending_jobs: i64 = self
            .connection
            .query_row(
                "SELECT count(*) FROM index_jobs WHERE status='running'",
                [],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        Ok(IndexStatus {
            project_id: self.project_id.clone(),
            root: self.root.to_string_lossy().into_owned(),
            index_dir: self.index_dir.to_string_lossy().into_owned(),
            index_version: generation.version,
            files: count("files")?,
            chunks: count("chunks")?,
            vectors: count("chunk_embeddings")?,
            cache_entries: count("embedding_cache")?,
            embedding_fingerprint: generation.fingerprint,
            last_indexed_at: Some(generation.created_at),
            pending_jobs: usize::try_from(pending_jobs).map_err(storage_error)?,
        })
    }
}

struct PathFilter {
    prefix: Option<String>,
    glob: Option<GlobMatcher>,
}

impl PathFilter {
    fn new(filter: Option<&str>) -> Result<Self> {
        let Some(filter) = filter else {
            return Ok(Self {
                prefix: None,
                glob: None,
            });
        };
        validate_relative_path(filter)?;
        let filter = filter.replace('\\', "/");
        if filter.contains(['*', '?', '[', '{']) {
            let glob = Glob::new(&filter)
                .map_err(|error| Error::InvalidInput(error.to_string()))?
                .compile_matcher();
            Ok(Self {
                prefix: None,
                glob: Some(glob),
            })
        } else {
            Ok(Self {
                prefix: Some(filter),
                glob: None,
            })
        }
    }

    fn matches(&self, path: &str) -> bool {
        self.glob.as_ref().is_none_or(|glob| glob.is_match(path))
            && self
                .prefix
                .as_ref()
                .is_none_or(|prefix| path.starts_with(prefix))
    }

    fn is_unrestricted(&self) -> bool {
        self.prefix.is_none() && self.glob.is_none()
    }
}

fn fts_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .take(24)
        .map(|token| format!("\"{}\"", token.chars().take(128).collect::<String>()))
        .collect();
    (!tokens.is_empty()).then(|| tokens.join(" OR "))
}

fn escaped_like(text: &str) -> String {
    text.replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_")
}

impl LexicalIndex for Store {
    fn lexical_search(
        &self,
        query: &str,
        limit: usize,
        path_filter: Option<&str>,
    ) -> Result<Vec<RankedChunk>> {
        let Some(expression) = fts_expression(query) else {
            return Ok(Vec::new());
        };
        let filter = PathFilter::new(path_filter)?;
        // Rank the narrow FTS rows first; a path filter is applied while loading,
        // so only then is the ranked list left unbounded.
        let sql_limit = if filter.is_unrestricted() {
            sqlite_integer(limit)?
        } else {
            -1
        };
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT rowid, bm25(chunk_fts, 2.0, 5.0, 1.0) AS score FROM chunk_fts \
                 WHERE chunk_fts MATCH ?1 ORDER BY score, rowid LIMIT ?2",
            )
            .map_err(storage_error)?;
        let hits = statement
            .query_map(params![expression, sql_limit], |row| {
                Ok((row.get::<_, i64>(0)?, -row.get::<_, f64>(1)?))
            })
            .map_err(storage_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        self.load_ranked(hits, &filter, limit)
    }

    fn symbol_search(&self, request: &SymbolRequest) -> Result<Vec<RankedChunk>> {
        request.validate()?;
        let filter = PathFilter::new(request.path_filter.as_deref())?;
        let name = request.name.to_lowercase();
        let pattern = format!("%{}%", escaped_like(&name));
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT seq, symbol, path, kind FROM chunks WHERE symbol IS NOT NULL \
                 AND (lower(symbol) LIKE ?1 ESCAPE '!' OR lower(path) LIKE ?1 ESCAPE '!')",
            )
            .map_err(storage_error)?;
        let mut rows = statement.query([pattern]).map_err(storage_error)?;
        let mut hits: Vec<(i64, f64)> = Vec::new();
        while let Some(row) = rows.next().map_err(storage_error)? {
            let path: String = row.get(2).map_err(storage_error)?;
            let kind: String = row.get(3).map_err(storage_error)?;
            if !filter.matches(&path) || request.kind.as_ref().is_some_and(|wanted| wanted != &kind)
            {
                continue;
            }
            let symbol = row
                .get::<_, String>(1)
                .map_err(storage_error)?
                .to_lowercase();
            let leaf = symbol
                .rsplit("::")
                .next()
                .unwrap_or(&symbol)
                .rsplit('.')
                .next()
                .unwrap_or(&symbol);
            let score = if symbol == name || leaf == name {
                100.0
            } else if leaf.starts_with(&name) {
                75.0
            } else if symbol.contains(&name) {
                50.0
            } else {
                25.0
            };
            hits.push((row.get::<_, i64>(0).map_err(storage_error)?, score));
        }
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        hits.truncate(request.limit);
        self.load_ranked(hits, &PathFilter::new(None)?, request.limit)
    }
}

pub fn validate_vector(vector: &[f32], dimensions: usize) -> Result<()> {
    if dimensions == 0
        || vector.len() != dimensions
        || vector.iter().any(|value| !value.is_finite())
    {
        return Err(Error::ModelIncompatible(
            "invalid vector dimensions or non-finite values".into(),
        ));
    }
    let norm: f64 = vector.iter().map(|value| (*value as f64).powi(2)).sum();
    if !norm.is_finite() || norm <= 1e-24 {
        return Err(Error::ModelIncompatible(
            "zero or invalid vector norm".into(),
        ));
    }
    Ok(())
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_vector(blob: &[u8], dimensions: usize) -> Result<Vec<f32>> {
    if dimensions.checked_mul(4) != Some(blob.len()) {
        return Err(Error::Storage("corrupt vector blob length".into()));
    }
    Ok(blob
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect())
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f64> {
    validate_vector(left, right.len())?;
    validate_vector(right, left.len())?;
    let mut dot = 0.0;
    let mut norm_left = 0.0;
    let mut norm_right = 0.0;
    for (&left, &right) in left.iter().zip(right) {
        let (left, right) = (left as f64, right as f64);
        dot += left * right;
        norm_left += left * left;
        norm_right += right * right;
    }
    Ok((dot / (norm_left.sqrt() * norm_right.sqrt())).clamp(-1.0, 1.0))
}

impl VectorIndex for Store {
    fn vector_search(
        &self,
        query: &[f32],
        fingerprint: &str,
        limit: usize,
        path_filter: Option<&str>,
    ) -> Result<Vec<RankedChunk>> {
        let generation = self.current_generation()?.ok_or(Error::IndexMissing)?;
        let current = generation
            .fingerprint
            .ok_or_else(|| Error::ModelIncompatible("this index has no embeddings".into()))?;
        if current.key() != fingerprint {
            return Err(Error::ModelIncompatible(
                "query and index fingerprints differ".into(),
            ));
        }
        validate_vector(query, current.dimensions)?;
        let filter = PathFilter::new(path_filter)?;
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT c.seq, c.path, e.dimensions, e.vector FROM chunk_embeddings ce \
                 JOIN chunks c ON c.id=ce.chunk_id \
                 JOIN embedding_cache e ON e.fingerprint=ce.fingerprint AND e.input_hash=ce.input_hash \
                 WHERE ce.fingerprint=?1",
            )
            .map_err(storage_error)?;
        let mut rows = statement.query([fingerprint]).map_err(storage_error)?;
        // Exact cosine scan keeping only the best `limit` hits.
        let mut hits: Vec<(i64, f64)> = Vec::with_capacity(limit.saturating_add(1));
        while let Some(row) = rows.next().map_err(storage_error)? {
            if !filter.matches(&row.get::<_, String>(1).map_err(storage_error)?) {
                continue;
            }
            let dimensions = usize::try_from(row.get::<_, i64>(2).map_err(storage_error)?)
                .map_err(storage_error)?;
            if dimensions != current.dimensions {
                return Err(Error::ModelIncompatible(
                    "stored vector dimensions differ".into(),
                ));
            }
            let vector = decode_vector(
                &row.get::<_, Vec<u8>>(3).map_err(storage_error)?,
                dimensions,
            )?;
            let score = cosine_similarity(query, &vector)?;
            if hits.len() < limit || hits.last().is_some_and(|last| score > last.1) {
                hits.push((row.get(0).map_err(storage_error)?, score));
                hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                hits.truncate(limit);
            }
        }
        drop(rows);
        self.load_ranked(hits, &PathFilter::new(None)?, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solascope_core::{hash_text, EMBEDDING_TEMPLATE_VERSION};

    fn update(root: &Path, path: &str, name: &str) -> FileUpdate {
        let content = format!("fn {name}() {{ println!(\"hello\"); }}");
        let id = project_id(root);
        let chunk = Chunk {
            id: hash_text(&format!("{path}:{name}")),
            entity_id: hash_text(name),
            project_id: id,
            relative_path: path.into(),
            language: "rust".into(),
            symbol: Some(name.into()),
            kind: "function".into(),
            signature: Some(format!("fn {name}()")),
            parent_symbol: None,
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: content.len(),
            content: content.clone(),
            content_hash: hash_text(&content),
            embedding_text: content.clone(),
            embedding_hash: hash_text(&content),
            fallback: false,
        };
        FileUpdate {
            source: SourceFile {
                relative_path: path.into(),
                absolute_path: root.join(path),
                hash: hash_text(&content),
                content,
            },
            chunks: vec![chunk],
        }
    }

    fn fingerprint() -> ModelFingerprint {
        ModelFingerprint {
            provider: "test".into(),
            model: "test".into(),
            endpoint: "local".into(),
            dimensions: 2,
            normalization: "l2".into(),
            template_version: EMBEDDING_TEMPLATE_VERSION.into(),
        }
    }

    #[test]
    fn transactions_keep_fts_and_deletions_consistent() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".solascope");
        let lock = WriterLock::acquire(&directory).unwrap();
        let mut store = Store::open(&directory, root.path(), true).unwrap();
        let job = store.begin_job(&lock).unwrap();
        store
            .commit_index(
                &lock,
                &[update(root.path(), "src/lib.rs", "greet")],
                &[],
                None,
                &[],
                false,
                job,
                3,
            )
            .unwrap();
        assert_eq!(
            store
                .lexical_search("greet() OR evil:*", 10, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.status().unwrap().files, 1);
        let job = store.begin_job(&lock).unwrap();
        store
            .commit_index(&lock, &[], &["src/lib.rs".into()], None, &[], false, job, 3)
            .unwrap();
        assert!(store.lexical_search("greet", 10, None).unwrap().is_empty());
        store.integrity_check().unwrap();
    }

    #[test]
    fn missing_embeddings_roll_back_every_table() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".solascope");
        let lock = WriterLock::acquire(&directory).unwrap();
        let mut store = Store::open(&directory, root.path(), true).unwrap();
        let job = store.begin_job(&lock).unwrap();
        assert!(store
            .commit_index(
                &lock,
                &[update(root.path(), "lib.rs", "greet")],
                &[],
                Some(&fingerprint()),
                &[],
                false,
                job,
                3
            )
            .is_err());
        assert!(store.file_hashes().unwrap().is_empty());
        assert!(store.current_generation().unwrap().is_none());
        assert!(store.lexical_search("greet", 10, None).unwrap().is_empty());
    }

    #[test]
    fn vectors_are_isolated_by_fingerprint_and_gc_is_safe() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".solascope");
        let lock = WriterLock::acquire(&directory).unwrap();
        let mut store = Store::open(&directory, root.path(), true).unwrap();
        let file = update(root.path(), "lib.rs", "greet");
        let vector = CachedEmbedding {
            input_hash: file.chunks[0].embedding_hash.clone(),
            vector: vec![1.0, 0.0],
        };
        let job = store.begin_job(&lock).unwrap();
        store
            .commit_index(
                &lock,
                &[file],
                &[],
                Some(&fingerprint()),
                &[vector],
                false,
                job,
                3,
            )
            .unwrap();
        assert_eq!(
            store
                .vector_search(&[1.0, 0.0], &fingerprint().key(), 10, None)
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .vector_search(&[1.0, 0.0], "different", 10, None)
            .is_err());
        assert_eq!(store.prune_unused_cache(&lock).unwrap(), 0);
        assert_eq!(store.status().unwrap().vectors, 1);
    }

    #[test]
    fn writer_lock_rejects_a_second_writer() {
        let root = tempfile::tempdir().unwrap();
        let first = WriterLock::acquire(root.path()).unwrap();
        assert!(matches!(
            WriterLock::acquire(root.path()),
            Err(Error::WriterBusy)
        ));
        drop(first);
        assert!(WriterLock::acquire(root.path()).is_ok());
    }

    #[test]
    fn read_only_open_does_not_create_an_index() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            Store::open(&root.path().join("missing"), root.path(), false),
            Err(Error::IndexMissing)
        ));
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn refuses_a_different_project_root() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let directory = root.path().join(".solascope");
        Store::open(&directory, root.path(), true).unwrap();
        assert!(Store::open(&directory, other.path(), true).is_err());
    }

    #[test]
    fn invalid_vectors_are_rejected() {
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]).is_err());
        assert!(cosine_similarity(&[f32::NAN, 1.0], &[1.0, 0.0]).is_err());
        assert!((cosine_similarity(&[2.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-6);
        assert!(decode_vector(&[1, 2, 3], 1).is_err());
    }
}
