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
| MCP | Four read-only tools over stdio, sharing Engine with the CLI; typed output schemas, tool annotations, and per-call path/default overrides |
| Evaluation | Labeled-query quality metrics, indexing times, query percentiles, and Linux resident-memory sampling |

See [configuration](configuration.md), [architecture](architecture.md), and [recorded benchmarks](benchmarks.md) for details.

## Validation

Workspace tests, Clippy, formatting checks, and CLI end-to-end checks have run on Linux x86_64. HTTP model protocols use local mock services; MCP tests use both raw JSON-RPC and rmcp SDK clients. MCP coverage includes tool schemas and annotations, startup defaults and request overrides, independent reranking with `.env` authentication, current source visibility, index generation updates, and missing/outdated-index recovery. Embedding retry, cooldown, adaptive concurrency, RPM/TPM budget, and timeout behavior are exercised with local mocks and virtual time; these checks do not establish real provider throughput, quota behavior, or service-specific `Retry-After` behavior.

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --all-targets --locked --features local-rerank
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

Live model services, local ONNX inference, macOS/Windows execution, and commercial MCP clients have not been tested. Three-platform CI, packaging, and draft-release workflows are configured; remote runners, distribution packages, and GitHub Release creation still need validation.

### CI and packages

The workflow layout and draft-release convention are inspired by [zoxide's workflows](https://github.com/ajeetdsouza/zoxide/tree/2f4bcebaa62a44742280172c9b4c2753266716d0/.github/workflows), adapted to this workspace's stable Cargo toolchain. Releases reuse packages from successful CI runs.

- `ci.yml` runs formatting, Clippy, default workspace tests, and Python package tests on all three platforms. Optional `local-rerank` tests run independently on Linux.
- After all validation jobs pass, `ci.yml` calls the reusable `package.yml` workflow. It reads the native Rust target from the runner's compiler, builds both `solascope` and `sas`, verifies that their version output matches, and includes the public documentation and `SHA256SUMS`.
- Rust caches are separated by job/platform or build target and saved only for `main`; superseded CI runs are cancelled.
- Archive and release-collection tests cover contents, executable permissions on Unix, combined checksums, missing platforms, source-version mismatches, alias-version mismatches, invalid inputs, and symlink rejection. Both scripts use Python's standard library.
- `release.yml` verifies and collects the three CI artifacts before creating a draft GitHub Release. Only the draft-creation job has `contents: write`; preparation uses read-only permissions and never executes downloaded packages.
- Publishing to crates.io is not configured.

| Native platform | Runner | Archive |
| --- | --- | --- |
| Linux x86_64 (GNU) | `ubuntu-latest` | `.tar.gz` |
| macOS (runner architecture) | `macos-latest` | `.tar.gz` |
| Windows x86_64 (MSVC) | `windows-latest` | `.zip` |

### Release operations

1. Update the workspace version and relevant documentation, then push the release commit to `main` with a message starting with `chore(release)`, such as `chore(release): v0.1.0`.
2. Wait for **Test and package** to succeed. The `workflow_run` trigger then starts `release.yml` for that push. Both workflows must already exist on the default branch for this trigger to work.
3. Review the draft's tag, source commit, three platform archives, and combined `SHA256SUMS`, then publish it manually on GitHub. A version with a prerelease suffix also marks the draft as a prerelease.

To retry after a workflow failure, or select another successful `main` CI run, dispatch `release.yml` on `main` and supply its run ID. The source run must belong to this repository, use `.github/workflows/ci.yml`, come from a push or manual dispatch on `main`, and have completed successfully. Its source commit must still be in `main` history. For example, replacing the example ID with the chosen run's ID:

```bash
gh workflow run release.yml --ref main -f run_id=1234567890
```

The workflow reads the workspace version from that run's source commit, checks that all three native packages match it, and verifies each package's checksum and exact public-file allowlist. It copies the archives without extracting or executing them and combines all checksum entries. CI artifacts must still be available; expired artifacts require a new successful CI run. Linux uses GNU binaries, and the macOS archive follows the runner's native architecture; no additional cross-compiled targets are provided.

The draft uses tag `v<workspace version>` and targets the verified CI commit. An existing release with that tag causes a failure, including an existing draft; review that release directly instead of rerunning to overwrite it. An existing tag must resolve to the same commit and is never moved. The workflow does not automatically publish drafts.

Local validation covers workflow linting, ShellCheck and shell formatting, archive inspection, combined checksums, and isolated Python CLI execution with fixture packages. These checks do not establish successful remote artifact downloads, token permissions, draft creation, or macOS/Windows binary execution.

## Current limitations

- Vector retrieval uses an exact scan; there is no ANN backend.
- Parsing extracts syntax definitions without cross-file references or call graphs.
- Local embeddings require Ollama; in-process embeddings are not included.
- There is no multi-project aggregation, shared cache, daemon, quantization, or index compression.
- Reranking results are not persisted. Hybrid database retrieval channels run sequentially.
