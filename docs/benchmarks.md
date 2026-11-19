# Recorded benchmarks

These measurements used Linux x86_64, four logical CPUs, and Rust 1.99, with embeddings and reranking disabled. They indicate the scale of indexing and query costs, not latency guarantees. Different build profiles, chunking versions, and timing scopes are not directly comparable.

See the [benchmark guide](../benchmarks/README.md) for tool usage.

## Debug build

Measurements used `solascope-bench`, with source copied into separate directories and a fresh temporary index for each corpus. Each indexing stage ran once. Each query ran once before 30 repeated observations.

| Corpus | Files | Chunks | First index | Unchanged scan | Rebuild | VmHWM after rebuild |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Five-language fixture | 5 | 22 | 46.4 ms | 9.4 ms | 16.3 ms | 21.4 MiB |
| tokio 1.54.1 | 582 | 10 560 | 4.21 s | 0.365 s | 5.17 s | 58.7 MiB |
| Six Rust libraries and project source | 1073 | 26 966 | 14.23 s | 0.757 s | 12.50 s | 108.4 MiB |

The third corpus contains tokio 1.54.1, hyper 1.12.0, reqwest 0.12.28, rusqlite 0.43.0, clap_builder 4.6.7, rmcp 3.11.0, and this project's `apps/` and `crates/` directories. Its index occupied about 93.7 MiB after first indexing and 102.8 MiB after rebuilding.

VmHWM is the process-wide cumulative resident-memory high-water mark, rather than memory exclusive to a stage. Index timings are single observations; a faster rebuild does not establish a repeatable improvement.

### Queries

The following results use the largest corpus. P50/P95 use the 30 repeated observations after the first call. Timing covers retrieval, fusion, and source checks within `Engine::search`, excluding process startup and JSON output.

| Query | Mode | P50 | P95 |
| --- | --- | ---: | ---: |
| `spawn_blocking` | lexical | 20.9 ms | 22.7 ms |
| `connection closed before message completed` | lexical | 75.0 ms | 76.7 ms |
| `how is the runtime shut down gracefully` | lexical | 121.3 ms | 130.3 ms |
| `spawn_blocking` | hybrid, no models | 43.8 ms | 45.3 ms |
| `connection closed before message completed` | hybrid, no models | 95.7 ms | 101.5 ms |
| `how is the runtime shut down gracefully` | hybrid, no models | 141.0 ms | 149.0 ms |

Lexical uses FTS5; hybrid without models also includes the independent symbol channel. Semantic and hybrid+rerank were not measured. The smallest corpus used the fixture's 18 queries; both larger corpora used the three queries above.

A query file for the larger corpora can contain:

```json
[
  "spawn_blocking",
  "connection closed before message completed",
  "how is the runtime shut down gracefully"
]
```

```bash
cargo run -p solascope-bench -- perf --root /path/to/copied-corpus \
  --queries /path/to/queries.json --config /path/to/models-disabled.toml \
  --runs 30 --json
```

This command uses a debug build, matching the table. For performance comparisons, add `--release` and fix the corpus, query set, model configuration, and cache conditions.

## Earlier release build

An earlier v0.1.0 build indexed the same six Rust libraries and project source: 1069 files and 30 273 chunks. It used different chunking rules, and query timing included process startup, database opening, and JSON output.

| Operation | Time | Peak memory |
| --- | ---: | ---: |
| First index | 3.6 s | 109 MB |
| Unchanged scan | 0.25 s | 29 MB |
| Index after changing one file | 0.26 s | 28 MB |
| Full rebuild | 4.6 s | 108 MB |

| Query | P50 | P95 |
| --- | ---: | ---: |
| lexical: `spawn_blocking` | 21.4 ms | 22.2 ms |
| lexical: `connection closed before message completed` | 24.7 ms | 26.1 ms |
| lexical: `how is the runtime shut down gracefully` | 29.6 ms | 30.6 ms |
| hybrid, no models: same natural-language query | 29.8 ms | 30.7 ms |
| lexical: `shutdown`, restricted to tokio source | 24.2 ms | 25.3 ms |
| symbol: `spawn_blocking` | 16.5 ms | 17.2 ms |
| symbol: no match | 13.3 ms | 14.3 ms |

The database occupied about 103 MB for about 15 MiB of source. The index stores source bodies, embedding inputs, and FTS data without compression. Vector and reranking latency were not measured.

## Fixture quality

`tests/fixtures/multilang/` contains source files in five languages and 18 labeled queries, including five with expected line ranges.

```bash
cargo test -p solascope-engine --test quality -- --nocapture
```

| Mode | Top1 | Recall@5 | MRR@10 | Wrong-location rate |
| --- | ---: | ---: | ---: | ---: |
| lexical | 1.000 | 1.000 | 1.000 | 0 |
| hybrid, no models | 1.000 | 1.000 | 1.000 | 0 |

These queries check file, symbol, and line matching. The sample is small and query terms often overlap comments, so it does not establish semantic retrieval quality on real projects. Live-model comparisons, larger labeled corpora, and concurrent watch/CLI/MCP latency remain to be measured.
