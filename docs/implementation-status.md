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

### CI and packages

The workflow layout is inspired by [zoxide's workflows](https://github.com/ajeetdsouza/zoxide/tree/2f4bcebaa62a44742280172c9b4c2753266716d0/.github/workflows), adapted to this workspace's stable Cargo toolchain rather than Nix/nightly or publishing jobs.

- `ci.yml` runs formatting, Clippy, default workspace tests, and Python package tests on all three platforms. Optional `local-rerank` tests run independently on Linux.
- After all validation jobs pass, `ci.yml` calls the reusable `package.yml` workflow. It reads the native Rust target from the runner's compiler, builds both `solascope` and `sas`, verifies that their version output matches, and includes the public documentation and `SHA256SUMS`.
- Rust caches are separated by job/platform or build target and saved only for `main`; superseded CI runs are cancelled.
- Archive tests cover contents, executable permissions on Unix, checksums, alias-version mismatches, invalid inputs, and symlink rejection. The packaging script uses Python's standard library.
- Only workflow artifacts are uploaded. No crates.io or GitHub Release publication is configured.

| Native platform | Runner | Archive |
| --- | --- | --- |
| Linux x86_64 (GNU) | `ubuntu-latest` | `.tar.gz` |
| macOS (runner architecture) | `macos-latest` | `.tar.gz` |
| Windows x86_64 (MSVC) | `windows-latest` | `.zip` |

## Current limitations

- Vector retrieval uses an exact scan; there is no ANN backend.
- Parsing extracts syntax definitions without cross-file references or call graphs.
- Local embeddings require Ollama; in-process embeddings are not included.
- There is no multi-project aggregation, shared cache, daemon, quantization, or index compression.
- Reranking results are not persisted. Hybrid database retrieval channels run sequentially.
