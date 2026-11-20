# Architecture

## Workspace

| Package | Responsibility |
| --- | --- |
| `apps/solascope` | The `solascope` and `sas` entry points, arguments, output formatting, logging, and rmcp stdio server |
| `crates/solascope-core` | Shared types, errors, exit codes, and the `EmbeddingProvider`, `RerankProvider`, `MetadataStore`, `LexicalIndex`, and `VectorIndex` traits |
| `crates/solascope-config` | Layered TOML configuration, environment overrides, validation, and privacy constraints |
| `crates/solascope-scanner` | File discovery, ignore and sensitive-path filters, root-confined reads, and file watching |
| `crates/solascope-parser` | Tree-sitter definition extraction, chunking, and stable identifiers |
| `crates/solascope-models` | Ollama, generic/OpenAI-compatible, DashScope, and SiliconFlow HTTP protocols, plus optional FastEmbed reranking |
| `crates/solascope-storage` | SQLite migrations, transactional updates, vector caches, FTS5, exact cosine retrieval, and writer locks |
| `crates/solascope-engine` | Indexing, search, diagnostics, and watch orchestration |
| `benchmarks` | The separate `solascope-bench` tool for quality and performance evaluation through Engine |

The CLI, MCP server, and benchmark tool call the same `solascope_engine::Engine`. Chunking lives in parser, lexical and vector retrieval in storage, and pipeline orchestration in engine. Core defines the shared interfaces.

Cloned Engine instances share lazily initialized model adapters and a query-vector cache. Embedding requests made through the same shared provider adapter also share cooldown and adaptive concurrency state across indexing and queries. Independent provider or Engine instances and separate processes do not share that state. A local ONNX model is reused across requests. Models are disabled by default, so the core application can build and start without an inference service.

## Indexing pipeline

1. Acquire an exclusive lock in the index directory. A competing writer fails immediately with `WriterBusy` (exit code 5). On first use, create an index-local `.gitignore` without replacing an existing file.
2. Apply database migrations in order. Reads never migrate. Compare the configured embedding fingerprint with the published generation: `strict` rejects incompatible incremental writes; `fingerprint_policy = "rebuild"` converts an explicit indexing operation into a rebuild.
3. Compute a chunking signature from `CHUNKER_VERSION`, enabled languages, fallback policy, chunk budget, and overlap. A changed signature requires all files to be parsed again before publication.
4. Scan the project and compare source SHA-256 hashes with stored records to identify additions, changes, and deletions. Parse changed files in a bounded Rayon thread pool.
5. Look up vectors by fingerprint and full-input hash. Incremental runs can also reuse moved-file vectors through a path-independent key. Request embeddings for the remaining inputs.
6. Run model requests outside the database write transaction. Queue at most 128 inputs in groups aligned to the provider's effective batch size. HTTP batches complete out of order to keep bounded concurrency slots occupied; restore original input order before associating vectors with chunks. Embedding requests use shared rate-limit cooldown, adaptive concurrency, and optional RPM/TPM budgets. Validate vector counts, dimensions, and values before proceeding.
7. Reread changed files and verify their hashes, then recheck pending deletions. Discard the update if source revisions changed in the meantime; watch mode rescans and retries.
8. Update files, chunks, FTS, vector mappings, chunking metadata, and generation records in one SQLite transaction, then publish the new generation.

A failed update leaves the previous published snapshot available. Interrupted jobs remain in `index_jobs`; the next indexing run marks recovery and rescans. An explicit rebuild publishes a new generation even for an empty project. Reports distinguish exact cache hits, newly computed embeddings, and relocated embeddings.

## Chunking and source locations

Tree-sitter extracts definitions from Rust, Python, JavaScript/JSX, TypeScript/TSX, and Go. Unknown or disabled languages and files with invalid syntax use line windows when fallback is enabled, with `fallback` as the chunk kind. Parsing extracts syntax definitions rather than cross-file types or references.

With `fallback_policy = "skip"`, Engine stores an unparseable file's hash without chunks, removes its old definitions, and continues indexing other files. The report includes skipped-file counts and a warning. The parser's strict API still returns parse errors; thread-pool failures and other errors propagate.

- Definitions include directly adjacent comments, attributes, decorators, and declaration wrappers. Blank-line-separated comments stay independent.
- Classes, impl blocks, interfaces, and other containers retain their headers, extracted definitions, and remaining member text, including fields, constants, and comments.
- Long definitions prefer shallow syntax-node boundaries within the budget. These splits do not require overlap. Otherwise, chunking uses overlapping line windows, splitting oversized lines only at UTF-8 boundaries.
- `max_chunk_tokens × 4` estimates a byte budget; actual model token counts depend on the tokenizer.
- Each chunk stores an exact source slice, a 1-based inclusive line range, symbol, signature, parent information, and content hash. Coverage tests check character boundaries, source ranges, and retained source content.

## Identifiers, fingerprints, and caches

| Identifier or key | Purpose |
| --- | --- |
| Entity ID: hash of project, path, qualified symbol, and kind | Deduplicate final results |
| Chunk ID: entity ID plus chunk ordinal | Merge retrieval channels and associate FTS entries with vectors |
| Embedding-input hash | Exact cache lookup; the input template contains path, language, symbol, signature, and body, excluding line numbers |
| Embedding fingerprint | Bind vectors to provider, model, normalized endpoint, dimensions, normalization, and input-template/dimension-request policy |
| Path-independent reuse key | Match moved code using template version, language, symbol, signature, and body hash |
| Query-cache key | Fingerprint plus trimmed-query hash; process-local TTL of five minutes and capacity of 128 entries |

Moving a function within a file can retain an identical embedding input and reuse an exact cache entry. Moving or renaming a file changes the path in the input, but matching code can reuse an old-path vector. Storage marks this approximate reuse as `exact = false`; status reports it as `relocated_vectors`.

`index --rebuild` reuses exact entries and recomputes relocated vectors. Exact results can replace approximate entries, while approximate results cannot replace exact ones. Changing the reranker does not affect the vector index. Credentials, concurrency, timeout, retry delays, cooldown, RPM/TPM budgets, and other scheduling settings are excluded from the embedding fingerprint. The source template and vector fingerprint do not change when these limits change.

## Model protocols

Embedding and reranking providers are configured independently:

- `EmbeddingProvider::embed` returns ordered document vectors; `embed_query` returns one query vector. DashScope uses `text_type = "document"` and `"query"` respectively.
- Generic/OpenAI-compatible and SiliconFlow embeddings use `/v1/embeddings`, a flat `input`, and indexed `data`. Native DashScope uses `/api/v1/services/embeddings/text-embedding/text-embedding` with nested `input` and `parameters`.
- Generic, SiliconFlow, and compatible rerank APIs use flat requests and top-level `results`. Native DashScope uses `/api/v1/services/rerank/text-rerank/text-rerank` and nested `output.results`. Its compatible API can use `/compatible-api/v1/reranks`; request formats follow the vendor's API contract.
- `RerankProvider::limits` declares document-count and character limits. Scores must be finite and map to original document indices and IDs, without duplicates or out-of-range indices.
- Local reranking checks the complete query/document pair with the model's tokenizer before inference. HTTP services enforce their own token limits in addition to local character budgets.

HTTP requests have bounded timeouts and retries, response-size limits, and do not follow redirects. Loopback requests bypass proxies. Remote model use requires explicit privacy opt-in. See [support and limitations](implementation-status.md) for model validation status.

Embedding concurrency is a maximum, with all configured slots available initially. HTTP 429 responses and known rate-limit error codes inside HTTP 200 responses put all embedding requests using the same provider instance into shared cooldown across indexing and queries. A 429 reduces effective concurrency to one. In-flight successes from an older cooldown epoch do not count toward recovery; each three consecutive successes in the current epoch add one slot up to the configured maximum. Each ten consecutive successes reduce the base cooldown for the next event down to the configured minimum. The same concurrent cooldown event is applied only once, while a longer valid `Retry-After` may extend its wait. Independent provider instances and processes do not share cooldown state.

Fallback retries use `base × 2^attempt`, capped at the configured maximum, with bounded nonnegative jitter that never exceeds the cap. Repeated 429 events double the cooldown to that maximum, starting at the configured cooldown. `Retry-After` supports delta-seconds and HTTP-date and sets a minimum wait even when longer than the fallback cap. An unrepresentable rate-limit hint fails that request without permanently blocking the shared adapter. Authentication, balance, and permanent quota errors are not retried. Rerank keeps its existing independent retry strategy.

Optional RPM budgets space actual HTTP attempts smoothly by at least `60 / RPM` seconds. Optional TPM budgets use conservative UTF-8 input byte counts in a 60-second sliding window, split oversized batches, and reject a single text larger than the full budget before sending. Queueing does not precharge budget; every HTTP attempt, including failures and retries, counts. The timeout for each HTTP batch covers queueing, cooldown, sending, response reading, and retries; expiry cancels the future and releases queued or active capacity. Retry counts remain bounded independently by `retries`.

Model diagnostic events record only numeric durations, effective concurrency, and classified errors. They omit source text, keys, URLs, and raw HTTP headers.

## Search pipeline

1. Validate the query, result limit, path filter, and index chunking version. If vectors are needed, verify the fingerprint and compute or reuse a query vector.
2. Retrieve within one SQLite read transaction: lexical uses FTS5 BM25, semantic uses exact vectors, and hybrid uses lexical, symbol (exact, prefix, then substring), and optional vector channels. Hybrid database channels run sequentially against the same snapshot.
3. Merge by chunk ID and apply reciprocal rank fusion (RRF). Symbol-like queries prioritize exact definitions. Small weights adjust unnamed text and test/fixture/example paths; explicitly requested or filtered support code avoids the path penalty.
4. Reread candidate files. Mark modified, deleted, or concurrently changing sources as `stale`, and remove candidates blocked by current privacy or ignore rules. Reads remain confined to the project root.
5. Optionally send leading non-stale candidates to the reranker, within configured and provider limits. Validate score mappings, retain exact-symbol priority, and recheck source visibility and revisions after reranking.
6. Deduplicate by entity and return up to `limit` results, warnings, and stage timings.

`find_symbol` also supports limited edit-distance correction. Literal matches rank first, and corrected matches are marked `fuzzy`. Hybrid search does not use these fuzzy candidates.

If enabled by configuration, hybrid queries fall back to lexical and symbol retrieval when embeddings are temporarily unavailable or authentication fails. Semantic queries and model-incompatibility errors do not fall back. Reranker failures either preserve fused order or return an error according to `fail_open`.

`SearchResponse.timings` reports embedding, reranking, remaining local retrieval, and total wall-clock milliseconds. Local retrieval includes fusion and source checks as well as SQL. Query-vector cache hits set `embedding_ms = 0`, so warm-cache timings do not measure model inference latency.

## SQLite and migrations

SQLite uses WAL, foreign keys, and a single-writer file lock. Tables include `projects`, `files`, `chunks`, `chunk_fts` (FTS5), `embedding_cache`, `chunk_embeddings`, `index_generations`, `index_jobs`, and `meta` for chunking metadata. Triggers keep FTS rows in sync with chunks in the same transaction.

Vectors are stored as little-endian `f32` blobs and scanned for exact cosine similarity. Large indexes may need an ANN implementation behind `VectorIndex`, evaluated against the same query set.

`PRAGMA user_version` records the number of applied migrations, currently 2:

- `migrations/0001_initial.sql`: initial schema.
- `migrations/0002_metadata_and_relocation.sql`: chunking metadata, relocation keys, and exactness flags; existing chunks backfill the reuse keys.

Each migration's SQL, backfill, and version update share one transaction. New migrations are appended without rewriting published ones. Writers upgrade older indexes in place; read-only commands return `IndexOutdated` (exit code 3) and request an indexing run. A schema newer than the application is rejected with guidance to upgrade the program or choose another index directory.

Before migration or journal-mode changes, storage verifies that the database belongs to the project root. A wrong root cannot modify an older schema. Unrelated databases without a schema version are also rejected.

Generation retention keeps version records, not complete historical source snapshots. `doctor` checks database and FTS consistency; `--prune-cache` removes only unreferenced vectors and requires the writer lock.

## CLI, MCP, watching, and logging

- `sas` is a separately compiled entry point sharing the CLI implementation.
- `init` creates `.solascope.toml` in the user's `.solascope/` directory, without writing a project config or recording the current project root in user defaults.
- User and project configuration each load a sibling `.env`. Process variables take precedence, and file-relative paths resolve against the declaring directory. Credentials remain isolated per configuration instance without modifying the process environment; serialization and Debug hide their values. Model authentication and log redaction share this credential source.
- Indexes default to the project's `.solascope/` directory and can be relocated. The index directory cannot contain the project root; source reads always stay within that root.
- MCP exposes four read-only stdio tools. A separate `watch` or `index --watch` process publishes updates; WAL lets readers see complete generations.
- Watching starts the event stream before the initial scan. Debounced events trigger hash-based incremental scans, with source-revision checks before commit and retries on changes. An explicit `index` run can recover missed events.
- `--no-watch` overrides `index.watch`. Watch mode writes only the initial report to JSON stdout; later progress goes to logs.
- Logs default to stderr plus `logs/solascope.log` beside the active configuration. Runtime configuration metadata selects the directory without serializing it or changing the source root. `log.persist = false` disables the automatic file copy; explicit project-local `log.output` files remain supported. File writes stay within the chosen root, append only to files carrying the SolaScope log marker, exclude the selected log path from source access, and create Unix files with mode `0600`. HTTP retries and final CLI failures produce sanitized events.
- Configured API-key values are always redacted; general Bearer-token redaction is also enabled by default. Verbose logging enables SolaScope modules, while third-party protocol libraries stay quiet by default. Explicit `RUST_LOG` settings need care.
- `--probe` and `doctor --models` test configured models with fixed sentences rather than source files. MCP probe reports go to stderr. Startup probes require an explicit flag.

## Testing and limitations

HTTP protocol tests use local mock services to cover request formats, response mapping, errors, and fallback. Live models, ONNX inference, and cross-platform execution still need validation; see [support and limitations](implementation-status.md). Evaluation procedures are in the [benchmark guide](../benchmarks/README.md).
