CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE files (
    path TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    language TEXT NOT NULL
);
-- `seq` doubles as the FTS rowid, so FTS rows are deleted by key, not by scan.
CREATE TABLE chunks (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
    symbol TEXT,
    kind TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX chunks_path ON chunks(path);
-- Covering index: symbol lookups never touch the wide `data` column.
CREATE INDEX chunks_symbol ON chunks(symbol, path, kind);
CREATE VIRTUAL TABLE chunk_fts USING fts5(
    path,
    symbol,
    body
);
CREATE TRIGGER chunks_insert AFTER INSERT ON chunks BEGIN
    INSERT INTO chunk_fts(rowid, path, symbol, body)
    VALUES (new.seq, new.path, new.symbol, json_extract(new.data, '$.content'));
END;
CREATE TRIGGER chunks_delete AFTER DELETE ON chunks BEGIN
    DELETE FROM chunk_fts WHERE rowid = old.seq;
END;
CREATE TRIGGER chunks_update AFTER UPDATE ON chunks BEGIN
    DELETE FROM chunk_fts WHERE rowid = old.seq;
    INSERT INTO chunk_fts(rowid, path, symbol, body)
    VALUES (new.seq, new.path, new.symbol, json_extract(new.data, '$.content'));
END;
CREATE TABLE embedding_cache (
    fingerprint TEXT NOT NULL,
    input_hash TEXT NOT NULL,
    dimensions INTEGER NOT NULL CHECK(dimensions > 0),
    vector BLOB NOT NULL,
    PRIMARY KEY (fingerprint, input_hash)
);
CREATE TABLE chunk_embeddings (
    chunk_id TEXT PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    input_hash TEXT NOT NULL,
    FOREIGN KEY (fingerprint, input_hash)
        REFERENCES embedding_cache(fingerprint, input_hash)
);
CREATE TABLE index_generations (
    version INTEGER PRIMARY KEY,
    fingerprint TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE index_jobs (
    id INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL,
    error TEXT
);
