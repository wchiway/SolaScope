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

pub const DATABASE_NAME: &str = "index.sqlite3";

struct Migration {
    sql: &'static str,
    /// Data changes that need Rust; runs in the migration's transaction.
    after: Option<fn(&rusqlite::Transaction<'_>) -> Result<()>>,
}

/// Applied in order; a database's `user_version` is the number already applied.
/// Released migrations are never edited, only appended to.
const MIGRATIONS: &[Migration] = &[
    Migration {
        sql: include_str!("migrations/0001_initial.sql"),
        after: None,
    },
    Migration {
        sql: include_str!("migrations/0002_metadata_and_relocation.sql"),
        after: Some(backfill_relocation_keys),
    },
];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

fn migrate(connection: &mut Connection, from: i64) -> Result<()> {
    let from = usize::try_from(from).map_err(storage_error)?;
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(from) {
        let tx = connection.transaction().map_err(storage_error)?;
        tx.execute_batch(migration.sql).map_err(storage_error)?;
        if let Some(after) = migration.after {
            after(&tx)?;
        }
        tx.pragma_update(None, "user_version", sqlite_integer(index + 1)?)
            .map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
    }
    Ok(())
}

fn backfill_relocation_keys(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut select = tx
        .prepare(
            "SELECT ce.fingerprint, ce.input_hash, c.data FROM chunk_embeddings ce \
             JOIN chunks c ON c.id=ce.chunk_id",
        )
        .map_err(storage_error)?;
    let mut update = tx
        .prepare(
            "UPDATE embedding_cache SET relocation_key=?3 \
             WHERE fingerprint=?1 AND input_hash=?2 AND relocation_key IS NULL",
        )
        .map_err(storage_error)?;
    let mut rows = select.query([]).map_err(storage_error)?;
    while let Some(row) = rows.next().map_err(storage_error)? {
        let chunk: Chunk = serde_json::from_str(&row.get::<_, String>(2).map_err(storage_error)?)
            .map_err(storage_error)?;
        update
            .execute(params![
                row.get::<_, String>(0).map_err(storage_error)?,
                row.get::<_, String>(1).map_err(storage_error)?,
                chunk.relocation_key()
            ])
            .map_err(storage_error)?;
    }
    Ok(())
}

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
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
            {
                Error::WriterBusy
            } else {
                Error::Io(error)
            }
        })?;
        // The index stores source snippets: keep it out of version control
        // without touching the project's own ignore files.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(index_dir.join(".gitignore"))
        {
            Ok(mut ignore) => std::io::Write::write_all(&mut ignore, b"*\n")?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Error::Io(error)),
        }
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
    /// See [`Chunk::relocation_key`].
    pub relocation_key: String,
    pub vector: Vec<f32>,
    /// False when the vector was computed for the same code at another path.
    pub exact: bool,
}

/// Everything one index run publishes atomically.
pub struct IndexCommit<'a> {
    pub updates: &'a [FileUpdate],
    pub removed: &'a [String],
    pub fingerprint: Option<&'a ModelFingerprint>,
    pub vectors: &'a [CachedEmbedding],
    /// Publish a new generation even when no file changed.
    pub force_generation: bool,
    /// Signature of the chunking rules the stored chunks were produced with.
    pub chunking: &'a str,
    pub job: i64,
    pub retention: usize,
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
        if version > SCHEMA_VERSION {
            return Err(Error::Storage(format!(
                "index schema {version} is newer than this binary supports ({SCHEMA_VERSION}); \
                 upgrade SolaScope or use a new index directory"
            )));
        }
        let id = project_id(&root);
        let recorded: Option<(String, String)> = if version > 0 {
            connection
                .query_row("SELECT id, root FROM projects LIMIT 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
                .map_err(storage_error)?
        } else {
            None
        };
        // A rejected root must not upgrade someone else's index or change its
        // journal mode. Supported migrations keep the projects table stable.
        if recorded.as_ref().is_some_and(|(existing, existing_root)| {
            existing != &id || existing_root != &root.to_string_lossy()
        }) {
            return Err(Error::Storage(
                "this index belongs to another project root".into(),
            ));
        }
        if version < SCHEMA_VERSION {
            if !create {
                // Read-only commands never modify the index, not even to upgrade it.
                return Err(if version == 0 {
                    Error::IndexMissing
                } else {
                    Error::IndexOutdated(format!("schema {version}, expected {SCHEMA_VERSION}"))
                });
            }
            if version == 0 {
                let tables: i64 = connection.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                    [], |row| row.get(0),
                ).map_err(storage_error)?;
                if tables != 0 {
                    return Err(Error::Storage(
                        "refusing to migrate an unrelated database".into(),
                    ));
                }
            }
            migrate(&mut connection, version)?;
        }
        if create {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(storage_error)?;
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(storage_error)?;
        }
        match recorded {
            None if create => {
                connection
                    .execute(
                        "INSERT INTO projects(id, root, created_at) VALUES (?1, ?2, ?3)",
                        params![id, root.to_string_lossy(), sqlite_integer(timestamp())?],
                    )
                    .map_err(storage_error)?;
            }
            None => return Err(Error::IndexMissing),
            Some(_) => {}
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

    /// Cached vectors for exact embedding inputs. With `exact_only`, vectors
    /// that were reused from another path are treated as missing.
    pub fn cached_vectors(
        &self,
        fingerprint: &ModelFingerprint,
        hashes: &[String],
        exact_only: bool,
    ) -> Result<HashMap<String, Vec<f32>>> {
        self.lookup_vectors(
            "SELECT dimensions, vector FROM embedding_cache \
             WHERE fingerprint=?1 AND input_hash=?2 AND exact>=?3",
            fingerprint,
            hashes,
            i64::from(exact_only),
        )
    }

    /// Vectors of the same code at any path, keyed by relocation key.
    pub fn relocated_vectors(
        &self,
        fingerprint: &ModelFingerprint,
        relocation_keys: &[String],
    ) -> Result<HashMap<String, Vec<f32>>> {
        self.lookup_vectors(
            "SELECT dimensions, vector FROM embedding_cache \
             WHERE fingerprint=?1 AND relocation_key=?2 AND exact>=?3 \
             ORDER BY exact DESC, input_hash LIMIT 1",
            fingerprint,
            relocation_keys,
            0,
        )
    }

    fn lookup_vectors(
        &self,
        sql: &str,
        fingerprint: &ModelFingerprint,
        keys: &[String],
        minimum_exact: i64,
    ) -> Result<HashMap<String, Vec<f32>>> {
        let mut result = HashMap::new();
        let mut statement = self.connection.prepare(sql).map_err(storage_error)?;
        let fingerprint_key = fingerprint.key();
        for key in keys {
            if result.contains_key(key) {
                continue;
            }
            let value: Option<(i64, Vec<u8>)> = statement
                .query_row(params![fingerprint_key, key, minimum_exact], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
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
                result.insert(key.clone(), vector);
            }
        }
        Ok(result)
    }

    /// A single index-wide value, such as the chunking signature.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(storage_error)
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

    pub fn commit_index(
        &mut self,
        lock: &WriterLock,
        commit: IndexCommit<'_>,
    ) -> Result<IndexGeneration> {
        self.verify_lock(lock)?;
        let IndexCommit {
            updates,
            removed,
            fingerprint,
            vectors,
            force_generation,
            chunking,
            job,
            retention,
        } = commit;
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
                // A vector computed from the exact input replaces one that was
                // reused from another path, never the other way round.
                tx.prepare_cached(
                    "INSERT INTO embedding_cache(fingerprint, input_hash, dimensions, vector, relocation_key, exact) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(fingerprint, input_hash) DO UPDATE SET \
                     vector=excluded.vector, relocation_key=excluded.relocation_key, exact=1 \
                     WHERE excluded.exact=1 AND embedding_cache.exact=0",
                )
                .map_err(storage_error)?
                .execute(params![
                    key,
                    item.input_hash,
                    sqlite_integer(fingerprint.dimensions)?,
                    blob,
                    item.relocation_key,
                    item.exact
                ])
                .map_err(storage_error)?;
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
            "INSERT INTO meta(key, value) VALUES ('chunking', ?1) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [chunking],
        )
        .map_err(storage_error)?;
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
            relocated_vectors: count(
                "chunk_embeddings ce JOIN embedding_cache e \
                 ON e.fingerprint=ce.fingerprint AND e.input_hash=ce.input_hash WHERE e.exact=0",
            )?,
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

/// Builds an FTS5 query from free text. Besides every token it adds adjacent
/// pairs and triples, both as phrases (prose and `snake_case`) and joined
/// (`camelCase`): "shut down" also reaches `shutdown`, "read session" reaches
/// `readSession`, and an exact error message outranks scattered words.
fn fts_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .take(24)
        .map(|token| token.chars().take(128).collect())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let mut terms: Vec<String> = tokens.clone();
    for width in 2..=3 {
        for window in tokens.windows(width).take(12) {
            terms.push(window.join(" "));
            terms.push(window.concat());
        }
    }
    if tokens.len() > 3 {
        terms.push(tokens.join(" "));
    }
    let mut seen = std::collections::HashSet::new();
    terms.retain(|term| seen.insert(term.to_lowercase()));
    // Tokens hold only alphanumerics and underscores, so quoting is safe.
    Some(
        terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// Optimal string alignment distance (insert, delete, substitute, swap of
/// neighbours), or `None` when it exceeds `max`.
fn edit_distance_within(left: &[char], right: &[char], max: usize) -> Option<usize> {
    if left.len().abs_diff(right.len()) > max {
        return None;
    }
    let mut before_previous = vec![0; right.len() + 1];
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for i in 1..=left.len() {
        current[0] = i;
        let mut row_minimum = i;
        for j in 1..=right.len() {
            let substitution = previous[j - 1] + usize::from(left[i - 1] != right[j - 1]);
            let mut value = substitution.min(previous[j] + 1).min(current[j - 1] + 1);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                value = value.min(before_previous[j - 2] + 1);
            }
            current[j] = value;
            row_minimum = row_minimum.min(value);
        }
        if row_minimum > max {
            return None;
        }
        std::mem::swap(&mut before_previous, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    Some(previous[right.len()]).filter(|distance| *distance <= max)
}

/// The last segment of a qualified name such as `Type::method` or `Class.method`.
fn leaf_name(symbol: &str) -> &str {
    let leaf = symbol.rsplit("::").next().unwrap_or(symbol);
    leaf.rsplit('.').next().unwrap_or(leaf)
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

    fn symbol_search(&self, request: &SymbolRequest, fuzzy: bool) -> Result<Vec<RankedChunk>> {
        request.validate()?;
        let filter = PathFilter::new(request.path_filter.as_deref())?;
        let name = request.name.to_lowercase();
        let wanted = |path: &str, kind: &str| {
            filter.matches(path) && request.kind.as_ref().is_none_or(|wanted| wanted == kind)
        };
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
            if !wanted(&path, &kind) {
                continue;
            }
            let symbol = row
                .get::<_, String>(1)
                .map_err(storage_error)?
                .to_lowercase();
            let leaf = leaf_name(&symbol);
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
        drop(rows);
        // Tolerate a typo in an identifier, but only to fill otherwise empty
        // slots: near misses always rank below every literal match.
        let query: Vec<char> = leaf_name(&name).chars().collect();
        if fuzzy
            && hits.len() < request.limit
            && query.len() >= 4
            && !name.contains(char::is_whitespace)
        {
            let tolerance = if query.len() < 8 { 1 } else { 2 };
            let found: std::collections::HashSet<i64> = hits.iter().map(|hit| hit.0).collect();
            let mut statement = self
                .connection
                .prepare_cached(
                    "SELECT seq, symbol, path, kind FROM chunks WHERE symbol IS NOT NULL",
                )
                .map_err(storage_error)?;
            let mut rows = statement.query([]).map_err(storage_error)?;
            while let Some(row) = rows.next().map_err(storage_error)? {
                let seq: i64 = row.get(0).map_err(storage_error)?;
                if found.contains(&seq) {
                    continue;
                }
                let symbol = row
                    .get::<_, String>(1)
                    .map_err(storage_error)?
                    .to_lowercase();
                let leaf: Vec<char> = leaf_name(&symbol).chars().collect();
                let Some(distance) = edit_distance_within(&query, &leaf, tolerance) else {
                    continue;
                };
                let path: String = row.get(2).map_err(storage_error)?;
                let kind: String = row.get(3).map_err(storage_error)?;
                if wanted(&path, &kind) {
                    hits.push((seq, 20.0 - 5.0 * distance as f64));
                }
            }
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
        let embedding_text = format!("path: {path}\n{content}");
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
            embedding_hash: hash_text(&embedding_text),
            embedding_text,
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

    fn vector(chunk: &Chunk, values: [f32; 2], exact: bool) -> CachedEmbedding {
        CachedEmbedding {
            input_hash: chunk.embedding_hash.clone(),
            relocation_key: chunk.relocation_key(),
            vector: values.to_vec(),
            exact,
        }
    }

    fn commit(
        store: &mut Store,
        lock: &WriterLock,
        updates: &[FileUpdate],
        removed: &[String],
        fingerprint: Option<&ModelFingerprint>,
        vectors: &[CachedEmbedding],
    ) -> Result<IndexGeneration> {
        let job = store.begin_job(lock)?;
        store.commit_index(
            lock,
            IndexCommit {
                updates,
                removed,
                fingerprint,
                vectors,
                force_generation: false,
                chunking: "test",
                job,
                retention: 3,
            },
        )
    }

    fn open(root: &Path) -> (PathBuf, WriterLock, Store) {
        let directory = root.join(".solascope");
        let lock = WriterLock::acquire(&directory).unwrap();
        let store = Store::open(&directory, root, true).unwrap();
        (directory, lock, store)
    }

    #[test]
    fn transactions_keep_fts_and_deletions_consistent() {
        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let file = update(root.path(), "src/lib.rs", "greet");
        commit(&mut store, &lock, &[file], &[], None, &[]).unwrap();
        assert_eq!(
            store
                .lexical_search("greet() OR evil:*", 10, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.status().unwrap().files, 1);
        assert_eq!(store.meta("chunking").unwrap().as_deref(), Some("test"));
        commit(&mut store, &lock, &[], &["src/lib.rs".into()], None, &[]).unwrap();
        assert!(store.lexical_search("greet", 10, None).unwrap().is_empty());
        store.integrity_check().unwrap();
    }

    #[test]
    fn missing_embeddings_roll_back_every_table() {
        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let file = update(root.path(), "lib.rs", "greet");
        assert!(commit(&mut store, &lock, &[file], &[], Some(&fingerprint()), &[]).is_err());
        assert!(store.file_hashes().unwrap().is_empty());
        assert!(store.current_generation().unwrap().is_none());
        assert!(store.lexical_search("greet", 10, None).unwrap().is_empty());
        assert!(store.meta("chunking").unwrap().is_none());
    }

    #[test]
    fn vectors_are_isolated_by_fingerprint_and_gc_is_safe() {
        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let file = update(root.path(), "lib.rs", "greet");
        let vectors = [vector(&file.chunks[0], [1.0, 0.0], true)];
        commit(
            &mut store,
            &lock,
            &[file],
            &[],
            Some(&fingerprint()),
            &vectors,
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
    fn moved_code_reuses_vectors_until_an_exact_one_replaces_them() {
        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let fingerprint = fingerprint();
        let original = update(root.path(), "old.rs", "greet");
        let moved = update(root.path(), "new.rs", "greet");
        let key = moved.chunks[0].relocation_key();
        let moved_hash = moved.chunks[0].embedding_hash.clone();
        assert_eq!(original.chunks[0].relocation_key(), key);
        assert_ne!(original.chunks[0].embedding_hash, moved_hash);
        let vectors = [vector(&original.chunks[0], [1.0, 0.0], true)];
        commit(
            &mut store,
            &lock,
            &[original],
            &[],
            Some(&fingerprint),
            &vectors,
        )
        .unwrap();

        let exact = |store: &Store, exact_only| {
            store
                .cached_vectors(&fingerprint, std::slice::from_ref(&moved_hash), exact_only)
                .unwrap()
        };
        assert!(exact(&store, false).is_empty());
        let reused = store
            .relocated_vectors(&fingerprint, std::slice::from_ref(&key))
            .unwrap();
        assert_eq!(reused[&key], vec![1.0, 0.0]);

        let relocated = [vector(&moved.chunks[0], [1.0, 0.0], false)];
        commit(
            &mut store,
            &lock,
            std::slice::from_ref(&moved),
            &["old.rs".into()],
            Some(&fingerprint),
            &relocated,
        )
        .unwrap();
        assert_eq!(store.status().unwrap().relocated_vectors, 1);
        assert_eq!(exact(&store, false)[&moved_hash], vec![1.0, 0.0]);
        // A rebuild asks for exact vectors only, so this one is recomputed.
        assert!(exact(&store, true).is_empty());

        let recomputed = [vector(&moved.chunks[0], [0.0, 1.0], true)];
        commit(
            &mut store,
            &lock,
            std::slice::from_ref(&moved),
            &[],
            Some(&fingerprint),
            &recomputed,
        )
        .unwrap();
        assert_eq!(store.status().unwrap().relocated_vectors, 0);
        assert_eq!(exact(&store, true)[&moved_hash], vec![0.0, 1.0]);
        // A reused vector never overwrites an exact one.
        commit(
            &mut store,
            &lock,
            &[moved],
            &[],
            Some(&fingerprint),
            &relocated,
        )
        .unwrap();
        assert_eq!(exact(&store, true)[&moved_hash], vec![0.0, 1.0]);
    }

    #[test]
    fn version_one_indexes_are_upgraded_in_place_by_a_writer() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".solascope");
        fs::create_dir_all(&directory).unwrap();
        let file = update(root.path(), "lib.rs", "greet");
        let chunk = &file.chunks[0];
        let fingerprint = fingerprint();
        {
            let connection = Connection::open(directory.join(DATABASE_NAME)).unwrap();
            connection.execute_batch(MIGRATIONS[0].sql).unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            let canonical = root.path().canonicalize().unwrap();
            connection
                .execute(
                    "INSERT INTO projects(id, root, created_at) VALUES (?1, ?2, 1)",
                    params![project_id(&canonical), canonical.to_string_lossy()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO files(path, hash, language) VALUES ('lib.rs', ?1, 'rust')",
                    [&file.source.hash],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO chunks(id, path, symbol, kind, data) VALUES (?1, 'lib.rs', 'greet', 'function', ?2)",
                    params![chunk.id, serde_json::to_string(chunk).unwrap()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO embedding_cache(fingerprint, input_hash, dimensions, vector) VALUES (?1, ?2, 2, ?3)",
                    params![fingerprint.key(), chunk.embedding_hash, encode_vector(&[1.0, 0.0])],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO chunk_embeddings(chunk_id, fingerprint, input_hash) VALUES (?1, ?2, ?3)",
                    params![chunk.id, fingerprint.key(), chunk.embedding_hash],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO index_generations(version, fingerprint, created_at) VALUES (7, ?1, 1)",
                    [serde_json::to_string(&fingerprint).unwrap()],
                )
                .unwrap();
        }
        // A reader reports the outdated index and leaves the file untouched.
        assert!(matches!(
            Store::open(&directory, root.path(), false),
            Err(Error::IndexOutdated(_))
        ));
        let lock = WriterLock::acquire(&directory).unwrap();
        let store = Store::open(&directory, root.path(), true).unwrap();
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let status = store.status().unwrap();
        assert_eq!(
            (
                status.index_version,
                status.files,
                status.vectors,
                status.relocated_vectors
            ),
            (7, 1, 1, 0)
        );
        assert_eq!(store.lexical_search("greet", 10, None).unwrap().len(), 1);
        let key = chunk.relocation_key();
        assert_eq!(
            store
                .relocated_vectors(&fingerprint, std::slice::from_ref(&key))
                .unwrap()[&key],
            vec![1.0, 0.0]
        );
        store.integrity_check().unwrap();
        drop(lock);
        assert!(Store::open(&directory, root.path(), false).is_ok());
    }

    #[test]
    fn a_foreign_root_is_rejected_before_its_old_schema_is_modified() {
        let index = tempfile::tempdir().unwrap();
        let owner = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let owner_root = owner.path().canonicalize().unwrap();
        let path = index.path().join(DATABASE_NAME);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("migrations/0001_initial.sql"))
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute(
                "INSERT INTO projects(id, root, created_at) VALUES (?1, ?2, 0)",
                params![project_id(&owner_root), owner_root.to_string_lossy()],
            )
            .unwrap();
        drop(connection);
        let error = Store::open(index.path(), other.path(), true).err().unwrap();
        assert!(error.to_string().contains("another project root"));
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let meta_tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='meta'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(meta_tables, 0);
        let journal: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
        drop(connection);
        let store = Store::open(index.path(), owner.path(), true).unwrap();
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn an_index_from_a_newer_version_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (directory, _lock, store) = open(root.path());
        store
            .connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(store);
        for create in [false, true] {
            let error = Store::open(&directory, root.path(), create).err().unwrap();
            assert!(error.to_string().contains("newer"), "{error}");
        }
    }

    #[test]
    fn phrases_and_joined_words_reach_identifiers() {
        let expression = fts_expression("Shut down the pool").unwrap();
        for term in ["\"Shut down\"", "\"Shutdown\"", "\"Shut down the pool\""] {
            assert!(expression.contains(term), "{expression}");
        }
        assert!(fts_expression("?? !!").is_none());

        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let files = [
            update(root.path(), "a.rs", "shutdown_now"),
            update(root.path(), "b.rs", "readSession"),
            update(root.path(), "c.rs", "unrelated"),
        ];
        commit(&mut store, &lock, &files, &[], None, &[]).unwrap();
        let top = |query: &str| {
            store.lexical_search(query, 10, None).unwrap()[0]
                .chunk
                .symbol
                .clone()
                .unwrap()
        };
        assert_eq!(top("shut down"), "shutdown_now");
        assert_eq!(top("read session"), "readSession");
        assert_eq!(top("shutdown now"), "shutdown_now");
    }

    #[test]
    fn typos_match_symbols_only_on_request_and_below_literal_matches() {
        assert_eq!(edit_distance_within(&['a', 'b'], &['b', 'a'], 1), Some(1));
        assert_eq!(
            edit_distance_within(&['a', 'b', 'c'], &['a', 'b', 'c'], 0),
            Some(0)
        );
        assert_eq!(edit_distance_within(&['a'], &['a', 'b', 'c'], 1), None);
        assert_eq!(leaf_name("Type::method"), "method");
        assert_eq!(leaf_name("Class.method"), "method");

        let root = tempfile::tempdir().unwrap();
        let (_, lock, mut store) = open(root.path());
        let files = [
            update(root.path(), "a.rs", "authenticate_user"),
            update(root.path(), "b.rs", "authorize_user"),
        ];
        commit(&mut store, &lock, &files, &[], None, &[]).unwrap();
        let search = |name: &str, fuzzy| {
            let request = SymbolRequest {
                name: name.into(),
                kind: None,
                path_filter: None,
                limit: 10,
            };
            store
                .symbol_search(&request, fuzzy)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.chunk.symbol.unwrap(), hit.score))
                .collect::<Vec<_>>()
        };
        assert!(search("authentcate_user", false).is_empty());
        assert_eq!(
            search("authentcate_user", true),
            vec![("authenticate_user".to_owned(), 15.0)]
        );
        // A literal match stays first; the near miss only fills the list.
        assert_eq!(search("authenticate_user", true)[0].1, 100.0);
        assert!(search("zzzz", true).is_empty());
    }

    #[test]
    fn writer_lock_rejects_a_second_writer_and_hides_the_index_from_git() {
        let root = tempfile::tempdir().unwrap();
        let first = WriterLock::acquire(root.path()).unwrap();
        assert!(matches!(
            WriterLock::acquire(root.path()),
            Err(Error::WriterBusy)
        ));
        let ignore = root.path().join(".gitignore");
        assert_eq!(fs::read_to_string(&ignore).unwrap(), "*\n");
        drop(first);
        fs::write(&ignore, "custom\n").unwrap();
        assert!(WriterLock::acquire(root.path()).is_ok());
        assert_eq!(fs::read_to_string(&ignore).unwrap(), "custom\n");
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
