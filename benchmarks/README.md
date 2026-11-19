# Quality and performance evaluation

`solascope-bench` calls the shared Engine directly. It is a separate evaluation tool, not part of the default CLI installation. Run it from source:

```bash
cargo run --release -p solascope-bench -- quality \
  --root tests/fixtures/multilang \
  --queries tests/fixtures/multilang/queries.json

cargo run --release -p solascope-bench -- perf \
  --root /path/to/repository --queries /path/to/queries.json --runs 30 --json
```

Output defaults to Markdown tables. `--json` returns one JSON document; progress, skip reasons, and sampling warnings go to stderr. Use a release build for performance measurements. Reports identify debug builds.

## Quality evaluation

```text
solascope-bench quality --root DIR --queries FILE [--config TOML]
                       [--limit 10] [--no-index] [--json]
```

By default, the tool incrementally updates the project index first. `--no-index` reuses an existing index. If it is missing or its schema/chunking version is outdated, run `solascope index` first.

The query file follows the format in `tests/fixtures/multilang/queries.json`:

```json
[
  { "category": "symbol", "query": "readSession", "path": "cache.ts", "symbol": "readSession" },
  { "category": "error", "query": "TOKEN_CACHE_MISS", "path": "cache.ts", "symbol": "readSession", "start_line": 8, "end_line": 15 }
]
```

`path` is project-relative. `symbol` matches the qualified name or its final component, such as `readSession` in `Class.readSession`. Optional line ranges are inclusive and start at 1.

Each available mode runs with its own Engine to keep query caches separate:

| Mode | Requirement |
| --- | --- |
| lexical | Always available |
| hybrid, without reranking | Always available; lexical and symbol retrieval when embeddings are disabled |
| semantic | A configured embedding provider |
| hybrid+rerank | A configured reranker; local ONNX also requires the `local-rerank` build feature |

Unavailable modes appear in `skipped_variants`. Configured modes with failed requests record errors for the affected queries.

### Metrics

Results are grouped by mode and query category:

- **Top1**: fraction of queries with the expected file and symbol in the first result.
- **Recall@5**: fraction with the expected file and symbol in the first five results.
- **MRR@10**: mean reciprocal rank of the expected file and symbol within the first ten results; a miss contributes 0.
- **Wrong-location rate**: fraction with a returned first result in the wrong file/symbol, or with a line range different from the annotation.
- **No-result rate**: fraction of successful searches returning no results.
- **Error count**: number of failed searches. These remain in metric denominators but do not count as wrong-location or no-result cases.

Ranking metrics compare files and symbols; wrong-location checks also compare annotated line ranges. Top1 can therefore be 1 while the wrong-location rate is nonzero. Reports list missed or incorrect queries and mean/P50/P95 search time for each mode.

A query file inside the source tree is excluded from scanning and results so the annotations do not participate in retrieval. With `--no-index`, use an index that already excluded the annotations; otherwise they may consume retrieval slots.

## Performance evaluation

```text
solascope-bench perf --root DIR [--queries FILE] [--config TOML]
                    [--runs 30] [--keep-index] [--json]
```

Every run uses a fresh temporary index, leaving the project's existing index and source files untouched. The temporary index is removed on completion. `--keep-index` retains it and includes its path in the report.

The tool measures:

1. First indexing: wall-clock time for scanning, parsing, model requests, and persistence.
2. Unchanged indexing: an incremental scan immediately after the first run.
3. Explicit rebuild: reparsing all files, reusing eligible exact-vector cache entries.
4. File, chunk, and new-embedding counts, plus index-file sizes at each stage.
5. Process peak resident memory sampled after each stage.
6. With `--queries`, a first call and `--runs` repeated calls for each query in each available mode.

Query files accept either the labeled objects above or an array of strings. Duplicate query strings are measured once. Without a query file, latency measurements are skipped.

**Search timing excludes process startup.** It covers Engine retrieval, fusion, source-location checks, optional model requests, and reranking. First-call time is reported separately. Repeated-call minimum/P50/P95/maximum exclude that first call. Repeated semantic queries may hit the process-local vector cache, so warm-cache times do not measure model inference latency.

On Linux, memory sampling reads `VmHWM` in KiB from `/proc/self/status`. This is the process-wide cumulative high-water mark, not stage-exclusive memory; threads and approximate kernel accounting can cause small fluctuations. Other platforms report the measurement as unavailable.

### Percentiles

The tool uses nearest-rank percentiles: sort observations and select the `ceil(p * n)`th value. Small samples still produce percentiles, with `p50_undersampled` / `p95_undersampled` flags and a warning. For example, P95 is the maximum with fewer than 20 samples; it does not establish production latency.

## Privacy and models

Built-in defaults disable models, allowing lexical and symbol evaluation without a service. The tool also reads user and project configuration. Use `--config` to fix model settings when comparing modes. Remote services require `privacy.remote_opt_in = true`; keys come from a sibling `.env` or process environment variables. A fresh temporary vector index has no cache and requests an embedding for every chunk, so check cost and transmission scope before running it.

For local reranking:

```bash
cargo run --release -p solascope-bench --features local-rerank -- quality \
  --root /path/to/repository --queries /path/to/queries.json --config /path/to/config.toml
```

## Validation

```bash
cargo test -p solascope-bench
cargo clippy -p solascope-bench --all-targets -- -D warnings
```

Tests cover metrics, percentiles, file/symbol/line matching, errors and skipped modes, temporary-index isolation, and CLI output structure. Mock responses validate the pipeline; live embeddings and rerankers need separate quality evaluation.
