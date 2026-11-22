# MCP integration

SolaScope provides a stdio MCP server that shares the CLI's search engine and index. It offers four read-only lookup tools and two explicit index-maintenance tools.

Run these commands in the project directory to build the index before starting the server:

```bash
solascope index
solascope mcp
```

Alternatively, start `solascope mcp` without pre-indexing and authorize an `index` tool call when the client requests one.

- No startup root or index path is required. The default project is the server's current working directory. An optional startup `--root` or process `SOLASCOPE__PROJECT__ROOT` changes that default.
- All six tools accept an optional `root` project directory, so a client can select a project on each call. Absolute paths are accepted; relative roots resolve from the server's working directory. Omitting `root` or passing `null` uses the startup default, even after calls to another project.
- A selected project's index defaults to `<root>/.solascope/`. `root` selects the source project, not a database file; `project.index_dir` can still override storage through normal configuration.
- Startup never creates, updates, or migrates an index. The four lookup tools also never do so; `index` and `reindex` explicitly perform one-shot index mutations. Clients must authorize these tools explicitly: authorization to search does not authorize writes. A separate `watch` / `index --watch` process is still needed for continuous updates.
- Stdout carries protocol messages only. Diagnostics go to stderr and, by default, `logs/solascope.log` beside the active configuration. `log.persist = false` disables the automatic copy; an explicit `log.output` can redirect traces to a protected file within the project.
- Each selected project loads its own configuration and sibling `.env`, including custom index storage, parser, privacy, models, and search defaults. Startup `--config` remains an explicit settings override for every project. Tool arguments override startup `--limit`, `--mode`, `--path`, and `--no-rerank`, which override project defaults.
- Search, symbol lookup, and status require an existing index. Missing indexes and outdated schemas or chunking rules can be recovered by an explicitly authorized `index` or `reindex` call, or by running CLI `index`. Context reads current source and works without an index.
- Each search or symbol call reads the latest committed index generation. MCP `index` and `reindex`, or a separate indexing process, can publish updates while the server stays running. Current ignore and sensitive-path checks apply on every call.

## Client configuration

Clients that use an `mcpServers` configuration can launch the server as follows. Configure the client to require explicit user approval for `index` and `reindex`; remote embedding providers may receive source code and may charge for requests. Remote-model opt-in enables configured provider calls but does not grant client authorization for index writes.

```json
{
  "mcpServers": {
    "solascope": {
      "command": "solascope",
      "args": ["mcp"]
    }
  }
}
```

For Claude Code:

```bash
claude mcp add solascope -- solascope mcp
```

If `solascope` is not on `PATH`, set `command` to the binary's absolute path. `sas` is an equivalent entry point. The current directory is the server process's working directory chosen by the client; an agent working elsewhere should pass that project's absolute `root`.

For example, tool arguments for searching another project can be:

```json
{ "root": "/abs/path/to/project", "query": "where are transactions committed", "mode": "lexical", "rerank": false }
```

Calling `index_status` with `{}` inspects the default project; `{ "root": "/abs/path/to/project" }` reports another project's resolved root and index directory. The same optional `root` argument selects the project for `index` and `reindex`. No tool changes the process working directory or the default for subsequent calls.

For example, after explicit client/user authorization, call `index` with `{}` to create or incrementally update the startup-default project, or `{ "root": "/abs/path/to/project" }` to select another existing directory. `reindex` accepts the same arguments and requests a full rebuild.

The server retains engines for up to eight additional projects to reuse model adapters and query caches. Project settings and `.env` are loaded on first access and again if an evicted project is revisited. Restart after configuration edits for consistent refresh; index generations and ignore files are checked on each call.

Remote-model keys can be stored in a `.env` beside the user configuration: `~/.solascope/.env` on Linux/macOS or `$env:USERPROFILE\.solascope\.env` on Windows (PowerShell syntax). Client-provided `env` values become process variables and take precedence. `--config FILE` also loads a `.env` beside FILE. Restart the server after editing the file. Keep keys out of arguments, URLs, and TOML values.

Remote models require `privacy.remote_opt_in = true`. Embedding and reranking are configured independently; see the [configuration reference](configuration.md).

End-to-end protocol tests use a raw JSON-RPC client and an rmcp SDK client. The client examples have not been tested with every commercial MCP client.

### Optional startup probe

```bash
solascope mcp --probe
```

`--probe` tests the startup default project's configured models with fixed sentences, without reading source code for probe input. Requests may incur model charges, and local models require their runtime to be available. A failed probe prevents normal startup. Reports go only to stderr. Without the flag, startup does not issue an inference probe.

## Tools

`tools/list` includes parameter descriptions, typed `outputSchema` values for successful structured results, and per-tool annotations. The four lookup tools (`search_code`, `find_symbol`, `get_code_context`, and `index_status`) have `readOnlyHint: true` and `destructiveHint: false`; `search_code` has `openWorldHint: true` because configured models may use a remote service, while the other three have `openWorldHint: false`. The `index` and `reindex` tools have `readOnlyHint: false`, `destructiveHint: true`, and `openWorldHint: true`; they are not advertised as idempotent. These annotations describe behavior and do not replace client/user authorization. Configured diagnostics can still write logs.

Use `search_code` to locate code from a question, keyword, or error; `find_symbol` for definitions and spelling correction; `get_code_context` to inspect current source; and `index_status` to inspect index counts and generation. Inspect warnings and `stale` before relying on indexed locations. Use `index` for a one-shot creation or incremental update, and `reindex` for an explicit full rebuild. These mutation calls share the same configuration, privacy checks, remote opt-in, writer lock, and atomic publication behavior as Engine indexing. Configured remote embeddings can send source code to a provider and incur charges; `privacy.remote_opt_in = true` permits remote model use but does not authorize the client to perform an index mutation. Tools do not offer watch or model-probe options.

The four lookup tools do not write or migrate indexes. The `index` and `reindex` tools can create/migrate/update them and require explicit authorization. All tools accept the same optional project selector:

| Parameter | Type | Description |
| --- | --- | --- |
| `root` | string | Existing project directory; omitted/null uses the startup default. Relative to the server's working directory; default index is `<root>/.solascope/` |

Source paths and `path_filter` are relative to the selected project. `root` may select another directory, while source reads still enforce that project's confinement, ignore rules, and sensitive-path exclusions.

### search_code

| Parameter | Type | Description |
| --- | --- | --- |
| `query` | string, required | 1–4096 UTF-8 bytes; cannot be blank |
| `limit` | integer | 1–100 |
| `mode` | `lexical` \| `semantic` \| `hybrid` | Defaults to the server's configured mode |
| `path_filter` | string | Project-relative path prefix or glob; omitted/null inherits the server filter, `""` clears it |
| `rerank` | boolean | Whether to use the configured reranker |

Lexical mode uses FTS5 without embeddings, semantic mode uses vectors, and hybrid mode combines lexical, symbol, and optional vector retrieval. Reranking is independent and can run in any mode; set `rerank: false` with `mode: "lexical"` to avoid all model calls.

With models disabled, hybrid search uses lexical and symbol retrieval and returns a warning; semantic search returns a model-unavailable error. If `search.semantic_fail_open` allows it, hybrid queries fall back when embeddings are temporarily unavailable or authentication fails. Fingerprint or dimension incompatibility does not fall back. Reranker failures preserve fused order or return an error according to `rerank.fail_open`.

### find_symbol

| Parameter | Type | Description |
| --- | --- | --- |
| `name` | string, required | Symbol name, qualified name, or fragment; 1–4096 bytes |
| `kind` | string | For example, `function`, `method`, `class`, or `struct` |
| `path_filter` | string | Project-relative path prefix or glob; omitted/null inherits the server filter, `""` clears it |
| `limit` | integer | 1–100 |

Matching prefers exact names, prefixes, substrings, then limited spelling correction. Corrected results use `fuzzy` in `match_sources` and are specific to this tool. No embedding or reranking requests are made.

### get_code_context

| Parameter | Type | Description |
| --- | --- | --- |
| `path` | string, required | Project-relative source path |
| `start_line`, `end_line` | integer | Inclusive range, starting at 1 |
| `line`, `before`, `after` | integer | Center line and surrounding lines; an alternative to the explicit range |

Returns `relative_path`, `start_line`, `end_line`, and `lines` (an array of `{ "line": 1, "text": "..." }`) from the current source file without requiring an index. Line numbers are 1-based and inclusive. Explicit ranges and center lines must lie within the file; before/after context is clipped at file boundaries. Requests exceeding the 200-line / 64-KiB bounds return an error; content is not silently truncated. Reads are subject to ignore and sensitive-path rules. Rejects `..`, absolute paths, Windows drive/UNC escapes, and symlinks outside the root. Include rules cannot override sensitive-path exclusions.

### index_status

Pass `{}` for the default project or `{ "root": "/abs/path/to/project" }` to select another. Returns the project ID, resolved root and index directory, index generation, file/chunk/vector counts, cache entries, `relocated_vectors`, embedding fingerprint, latest update time, and unfinished-job count.

`relocated_vectors` counts vectors reused after moving or renaming files. An explicit `reindex` recomputes these approximate entries using the current complete input. This tool checks schema and chunking compatibility, but does not probe models, check embedding fingerprint compatibility against the current configuration, or update the index.

### index

Accepts only the optional `root` selector described above and runs one indexing pass. It creates a missing index or incrementally updates changed files. A writer lock prevents concurrent writers. Engine prepares and validates the update before publishing files, chunks, vectors, metadata, and the generation atomically; a failed update leaves the previously published generation available. `rebuilt` reports the operation outcome and can be true if configuration requires rebuilding.

### reindex

Accepts only the optional `root` selector and runs one explicit full rebuild. It reparses all source and recomputes approximate embeddings reused after file moves, while reusing eligible exact embedding-cache entries. It uses the same writer lock and atomic publication as `index`. The report's `rebuilt` field is `true`.

Both tools return the shared typed `IndexReport` in `structuredContent`, also serialized as JSON text in `content`. Its fields are `project_id`, `index_version`, `files_total`, `files_updated`, `files_unchanged`, `files_deleted`, `chunks`, `embedded_chunks`, `cache_hits`, `relocated_embeddings`, `skipped_files`, `warnings`, `elapsed_ms`, and `rebuilt`. `embedded_chunks` counts newly computed embeddings; `cache_hits` counts embeddings reused from the cache; `relocated_embeddings` counts approximate vectors reused after moves; `elapsed_ms` is wall-clock duration; and `rebuilt` describes whether a rebuild occurred. Both tools run one pass even when `index.watch = true`; use the CLI watch command for continuous updates.

If the client cancels a mutation request, cancellation propagates to Engine and the handler awaits cleanup, including releasing the writer lock. Scanner/parser work already running may finish read-only work; if cancellation arrives before publication, the prior committed generation remains available. The MCP server suppresses the response for the canceled request, so clients generally receive no tool result for that call. A cancellation during or after publication must not be interpreted as rollback.

## Response structure

`search_code` and `find_symbol` return `structuredContent` with the following shape. Paths, scores, and timings are illustrative:

```json
{
  "query": "INSUFFICIENT_STOCK",
  "mode": "hybrid",
  "index_version": 2,
  "warnings": [],
  "timings": {
    "embedding_ms": 0.0,
    "retrieval_ms": 1.2,
    "rerank_ms": 0.0,
    "total_ms": 1.2
  },
  "results": [
    {
      "project_id": "example-project",
      "relative_path": "inventory.go",
      "symbol": "Inventory.Reserve",
      "kind": "method",
      "language": "go",
      "signature": "func (inventory *Inventory) Reserve(sku string, quantity int) error",
      "parent_symbol": "Inventory",
      "start_line": 13,
      "end_line": 20,
      "snippet": "...",
      "score": 0.0164,
      "rank": 1,
      "match_sources": ["lexical"],
      "index_version": 2,
      "stale": false
    }
  ]
}
```

- `score` orders results within a query. Values are not directly comparable across models or modes.
- `stale: true` means a file changed, was deleted, or changed during reading. Line numbers may be outdated; update the index. Deleted files have empty snippets. Results blocked by current privacy rules are removed.
- `match_sources` can contain `lexical`, `symbol`, `vector`, or `rerank`. Spelling corrections from `find_symbol` use `fuzzy`.
- `timings` uses wall-clock milliseconds. `retrieval_ms` includes local retrieval, fusion, and source checks, not just SQL. Unused model stages and query-vector cache hits report 0 for the corresponding model stage. Total time excludes client-side process startup.
- A query uses one SQLite read-transaction snapshot, so results share a generation. Source locations are checked again after reranking.

The same payload is also included as JSON text in `content` for clients without structured-result support. Cancellation of lookup calls interrupts waiting and discards the result. ONNX inference already running on a blocking thread cannot be forcibly preempted.

## Errors and recovery

Normal application errors return a tool result with `isError: true`; a failed tool call does not stop the server. Canceled requests are handled as described above and generally receive no tool result:

```json
{ "error": { "message": "invalid input: paths must be project-relative and cannot escape the root", "exit_code": 2 } }
```

| `exit_code` | Typical cause and action |
| --- | --- |
| 1 | Storage or I/O error; inspect the index with `doctor` |
| 2 | Invalid arguments, configuration, or privacy boundary; correct the request or settings |
| 3 | Missing index or outdated schema/chunking rules; call `index` or `reindex` with explicit authorization, or run CLI `index` |
| 4 | Unavailable model, authentication failure, or incompatible vector space; check the service and settings, then call `reindex` if a full rebuild is needed |
| 5 | Writer conflict, usually another MCP mutation, CLI `index`, or `watch` process; retry after it finishes |
| 130 | Operation cancelled |

Invalid parameter types and unknown fields also return `isError: true`, with an explanation in textual `content`. API keys and full remote error bodies are excluded from tool errors.

Use MCP `index` and `reindex` for explicitly authorized one-shot maintenance; use the CLI's `index`, `watch`, and `doctor` commands for maintenance and diagnostics, including continuous watching. MCP selects projects per call through its six tools above. See [support and limitations](implementation-status.md) for the tested scope; mutation tests use isolated temporary projects and local model mocks.
