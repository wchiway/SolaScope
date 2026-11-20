# Support and limitations

Current version: v0.1.0.

## Features

| Area | Support |
| --- | --- |
| CLI | `solascope` and `sas` share an implementation for indexing, search, symbols, context, diagnostics, and watching |
| Parsing | Definition extraction for Rust, Python, JavaScript/JSX, TypeScript/TSX, and Go; line-based chunks for other text |
| Storage | SQLite/FTS5, incremental updates, schema migrations, vector caches, a single writer, and read-transaction snapshots |
| Retrieval | Lexical, symbol, and exact-vector retrieval with RRF fusion; results include line ranges, match sources, and stale status |
| Embeddings | Ollama, generic/OpenAI-compatible APIs, DashScope, and SiliconFlow |
| Reranking | Generic compatible HTTP services, DashScope, SiliconFlow, and optional FastEmbed/ONNX |
| MCP | Four read-only tools over stdio, sharing Engine with the CLI |
| Evaluation | Labeled-query quality metrics, indexing times, query percentiles, and Linux resident-memory sampling |

See [configuration](configuration.md), [architecture](architecture.md), and [recorded benchmarks](benchmarks.md) for details.

## Validation

Workspace tests, Clippy, formatting checks, and CLI end-to-end checks have run on Linux x86_64. HTTP model protocols use local mock services; MCP tests use both raw JSON-RPC and rmcp SDK clients. Embedding retry, cooldown, adaptive concurrency, RPM/TPM budget, and timeout behavior are exercised with local mocks and virtual time; these checks do not establish real provider throughput, quota behavior, or service-specific `Retry-After` behavior.

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --all-targets --locked --features local-rerank
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

Live model services, local ONNX inference, macOS/Windows execution, and commercial MCP clients have not been tested. Three-platform CI and packaging workflows are configured; remote runners and distribution packages still need validation.

## Current limitations

- Vector retrieval uses an exact scan; there is no ANN backend.
- Parsing extracts syntax definitions without cross-file references or call graphs.
- Local embeddings require Ollama; in-process embeddings are not included.
- There is no multi-project aggregation, shared cache, daemon, quantization, or index compression.
- Reranking results are not persisted. Hybrid database retrieval channels run sequentially.
