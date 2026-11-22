# MCP integration

SolaScope provides a stdio MCP server that shares the CLI's search engine and index.

Run these commands in the project directory:

```bash
solascope index
solascope mcp
```

- No startup root or index path is required. The default project is the server's current working directory. An optional startup `--root` or process `SOLASCOPE__PROJECT__ROOT` changes that default.
- All four tools accept an optional `root` project directory, so an agent can select a project on each call. Absolute paths are accepted; relative roots resolve from the server's working directory. Omitting `root` or passing `null` uses the startup default, even after calls to another project.
- A selected project's index defaults to `<root>/.solascope/`. `root` selects the source project, not a database file; `project.index_dir` can still override storage through normal configuration.
- The server is read-only: it does not create, update, or migrate an index. Run `index` first, or keep a separate `watch` / `index --watch` process running.
- Stdout carries protocol messages only. Diagnostics go to stderr and, by default, `logs/solascope.log` beside the active configuration. `log.persist = false` disables the automatic copy; an explicit `log.output` can redirect traces to a protected file within the project.
- Each selected project loads its own configuration and sibling `.env`, including custom index storage, parser, privacy, models, and search defaults. Startup `--config` remains an explicit settings override for every project. Tool arguments override startup `--limit`, `--mode`, `--path`, and `--no-rerank`, which override project defaults.
- Search, symbol lookup, and status require an existing index. Missing indexes and outdated schemas or chunking rules produce errors with instructions to run `index` separately. Context reads current source and works without an index.
- Each search or symbol call reads the latest committed index generation. A separate indexing process can publish updates while the MCP server stays running. Current ignore and sensitive-path checks apply on every call.

## Client configuration

Clients that use an `mcpServers` configuration can launch the server as follows:

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

Calling `index_status` with `{}` inspects the default project; `{ "root": "/abs/path/to/project" }` reports another project's resolved root and index directory. No tool changes the process working directory or the default for subsequent calls.

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

`tools/list` includes parameter descriptions, a typed `outputSchema` for successful structured results, and `readOnlyHint: true` / `destructiveHint: false` annotations for all four tools. `search_code` has `openWorldHint: true` because configured models may use a remote service; the other tools have `openWorldHint: false`. Read-only means tools do not modify source or index data; configured diagnostics can still write logs.

Use `search_code` to locate code from a question, keyword, or error; `find_symbol` for definitions and spelling correction; `get_code_context` to inspect current source; and `index_status` to inspect index counts and generation. Inspect warnings and `stale` before relying on indexed locations.

All tools accept the same optional project selector:

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

`relocated_vectors` counts vectors reused after moving or renaming files. An explicit `index --rebuild` recomputes these approximate entries using the current complete input. This tool checks schema and chunking compatibility, but does not probe models, check embedding fingerprint compatibility against the current configuration, or update the index.

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

The same payload is also included as JSON text in `content` for clients without structured-result support. Cancellation interrupts waiting and discards the result. ONNX inference already running on a blocking thread cannot be forcibly preempted.

## Errors and recovery

Application errors return a tool result with `isError: true`; a failed query does not stop the server:

```json
{ "error": { "message": "invalid input: paths must be project-relative and cannot escape the root", "exit_code": 2 } }
```

| `exit_code` | Typical cause and action |
| --- | --- |
| 1 | Storage or I/O error; inspect the index with `doctor` |
| 2 | Invalid arguments, configuration, or privacy boundary; correct the request or settings |
| 3 | Missing index or outdated schema/chunking rules; run `index` separately |
| 4 | Unavailable model, authentication failure, or incompatible vector space; check the service and settings, then use `index --rebuild` if the fingerprint changed |
| 5 | Writer conflict, usually in a separate `index` or `watch` process |
| 130 | Operation cancelled |

Invalid parameter types and unknown fields also return `isError: true`, with an explanation in textual `content`. API keys and full remote error bodies are excluded from tool errors.

Use the CLI's `index`, `watch`, and `doctor` commands for maintenance and diagnostics. MCP selects projects per call through the four tools above. See [support and limitations](implementation-status.md) for the tested scope.
