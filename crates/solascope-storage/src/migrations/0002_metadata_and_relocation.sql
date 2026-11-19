-- Index-wide single values, such as the signature of the chunking rules.
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
-- `relocation_key` identifies an embedding input apart from its path, so a moved
-- file can reuse its vectors. `exact = 0` marks a vector reused that way; a
-- rebuild replaces it with one computed from the current input.
ALTER TABLE embedding_cache ADD COLUMN relocation_key TEXT;
ALTER TABLE embedding_cache ADD COLUMN exact INTEGER NOT NULL DEFAULT 1;
CREATE INDEX embedding_cache_relocation
    ON embedding_cache(fingerprint, relocation_key)
    WHERE relocation_key IS NOT NULL;
