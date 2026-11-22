# Support and limitations

Current version: v0.1.1.

## Features

| Area | Support |
| --- | --- |
| CLI | `solascope` and `sas` share an implementation for indexing, search, symbols, context, diagnostics, and watching |
| Parsing | Definition extraction for Rust, Python, JavaScript/JSX, TypeScript/TSX, and Go; line-based chunks for other text |
| Storage | SQLite/FTS5, incremental updates, schema migrations, vector caches, a single writer, and read-transaction snapshots |
| Retrieval | Lexical, symbol, and exact-vector retrieval with RRF fusion; results include line ranges, match sources, and stale status |
| Embeddings | Ollama, generic/OpenAI-compatible APIs, DashScope, and SiliconFlow |
| Reranking | Generic compatible HTTP services, DashScope, SiliconFlow, and optional FastEmbed/ONNX |
| MCP | Four read-only tools over stdio, sharing Engine with the CLI; typed output schemas, tool annotations, a current-directory default, and per-call project selection |
| Evaluation | Labeled-query quality metrics, indexing times, query percentiles, and Linux resident-memory sampling |

See [configuration](configuration.md), [architecture](architecture.md), and [recorded benchmarks](benchmarks.md) for details.

## Validation

Workspace tests, Clippy, formatting checks, and CLI end-to-end checks have run on Linux x86_64. HTTP model protocols use local mock services; MCP tests use both raw JSON-RPC and rmcp SDK clients. MCP coverage includes tool schemas and annotations, current-directory startup, per-call project selection, isolated project configuration and credentials, startup defaults and request overrides, independent reranking with `.env` authentication, current source visibility, index generation updates, and missing/outdated-index recovery. Embedding retry, cooldown, adaptive concurrency, RPM/TPM budget, and timeout behavior are exercised with local mocks and virtual time; these checks do not establish real provider throughput, quota behavior, or service-specific `Retry-After` behavior.

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --all-targets --locked --features local-rerank
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

The POSIX installer has run on Linux x86_64 with a locally packaged pair of real release CLI binaries, including repeated offline installation into a path with spaces, both installed version outputs, and preservation of configuration, shell profiles, and unrelated files. Shell fixtures cover checksums, exact tar names/types, failed extraction, upgrades/rollback, locks, symlinks, download fallback, version timeouts/output validation, and timer cleanup. Darwin/Intel/Apple Silicon/Rosetta selection is mocked on Linux; it does not establish actual macOS execution or BSD tar behavior. Online transport tests use mocks. The published `v0.1.0` Linux archive has also been downloaded, checked against its published checksum, and installed/reinstalled offline with both entry points verified.

Windows installer offline fixtures have run on Windows 11 with the OS-provided Windows PowerShell 5.1. They cover ZIP/checksum rejection, Unicode/space paths, repeated installation, upgrade/rollback, locked executables, serialization, configuration preservation, and PATH value generation, without writing the real User PATH. Fixture executables are .NET test programs. Separately, the published `v0.1.0` Windows ZIP has been downloaded, checksum-verified, and installed/reinstalled on a development Windows host with real User PATH unchanged. Legacy/current-layout tests cover version gating, missing/extra resources, executable-version mismatches, and upgrades. Some fixture runs encountered intermittent directory access-denied errors while moving freshly compiled test programs; subsequent standalone fixtures and the isolated real-ZIP installation passed. This does not establish the cause of that transient lock or clean-machine support; `v0.1.0` predates static-CRT verification. Live model services, local ONNX inference, clean-machine macOS/Windows CLI execution, online installer downloading, persistent User PATH changes, and commercial MCP clients have not been tested. Three-platform CI, packaging, and tag-triggered publication are configured; remote runners/packages, live DeepSeek summaries, and GitHub Release publication still need validation.

### Native self-update

`solascope update` and `sas update` share a native updater for the latest stable GitHub Release. It runs before project configuration, preserves indexes/model settings, verifies bounded downloads and SHA-256, stages and probes both entry points, serializes installation changes, and attempts rollback. `--check`, same-version `--force`, JSON output, and offline archive/checksum input are supported. Both names must be installed together; Cargo build directories and symlink/reparse paths are refused. Only binaries are replaced, using the default release features. Windows may retain old mapped binaries in a reported backup directory until existing processes exit.

Local-mock and filesystem tests cover metadata/redirect/checksum/archive rejection, version precedence/no downgrade, locks, staged probes, and rollback. On Linux, both copied CLI entry points have replaced themselves using a small native executable fixture, and the locally built release package has completed offline self-update through both names. Packaging CI is configured to exercise the real native package on Linux/macOS/Windows, including installer reuse after Windows updating. Windows/macOS self-update and live GitHub updating have not been executed locally; configured jobs alone do not establish their success.

### CI and packages

- `ci.yml` runs formatting, Clippy, default workspace tests, and Python release-tooling tests on all three platforms, plus POSIX installer fixtures/syntax checks on Linux/macOS and Windows PowerShell 5.1 installer fixtures on Windows. Optional `local-rerank` tests run independently on Linux. It can also be called by a release workflow.
- After validation passes, the reusable `package.yml` builds both CLI entry points for the native runner target, verifies matching versions, and packages the license and public documentation. Unix packages include `install.sh` and smoke-test repeated offline installation of the real native archive. Windows packages include `install.ps1`/`install.cmd`; Windows release builds enable static CRT, inspect ordinary/delay-loaded DLL imports, and smoke-test offline installation of the actual ZIP.
- Rust caches are separated by job/platform/build policy and saved only for `main`; superseded ordinary CI runs are cancelled. Release publication is serialized and is not cancelled during uploads.
- Archive and collection tests cover exact public content, executable permissions on Unix, combined archive/installer checksums, missing platforms, version mismatches, invalid inputs, and symlink rejection. Additional Python tests cover tag validation and bounded author-free changelog facts, PE dependencies, and DeepSeek Responses success/fallback/malformed/incomplete/error contracts. All Python release tooling uses the standard library.
- `release.yml` validates an existing version tag, runs CI on that commit, and collects only same-run assets. It optionally generates a description with DeepSeek, creates a temporary draft, verifies uploaded assets, and publishes automatically. Only publishing has `contents: write`; only description generation receives `DEEPSEEK_API_KEY`. Collection never executes downloaded package binaries; native installation smoke tests execute the binaries built in the same packaging job.
- Publishing to crates.io is not configured.

| Native platform | Runner | Archive |
| --- | --- | --- |
| Linux x86_64 (GNU) | `ubuntu-latest` | `.tar.gz` |
| macOS (runner architecture) | `macos-latest` | `.tar.gz` |
| Windows x86_64 (MSVC) | `windows-latest` | `.zip` |

### Release operations

Update the workspace version and public documentation, commit the changes into `main` history, and push a matching `v<workspace version>` tag. `release.yml` checks the tag/commit/version, invokes the reusable CI and package workflow, verifies the three archives, exports standalone Unix/Windows installers, and publishes only after all seven assets are uploaded and verified. `SHA256SUMS` covers the other six assets, and the two Unix archives must contain identical `install.sh` copies. An existing release (including a partial draft) is never overwritten, and tags are never moved.

Configure the optional repository secret `DEEPSEEK_API_KEY` and optional variable `DEEPSEEK_MODEL` (default `deepseek-flash`) to generate the description through `https://api.deepseek.com/responses`. Only bounded version/commit-subject metadata is sent; no source diffs or author identities. Missing credentials and provider failures produce a warning and retain factual notes. AI summaries are labeled and are not independent verification of behavior.

To retry, rerun the failed workflow or dispatch `release.yml` on its existing version tag, for example `gh workflow run release.yml --ref v0.1.1`. Branch dispatch is rejected. SemVer prerelease tags publish as prereleases and do not become latest. See the [release guide](releases.md) for configuration, failure recovery, privacy, and exact operations, and the [installation guide](installation.md) for Unix/Windows online and offline installation.

Local validation covers workflow/ShellCheck linting and shell formatting, package content and checksum inspection, isolated Python tag/release-note tests, real Linux archive installation, POSIX installer fixtures, and Windows PowerShell 5.1 installer fixtures. These checks do not establish successful remote artifact downloads, GitHub token permissions, live DeepSeek requests, publication, or clean-machine release binary execution.

## Current limitations

- Vector retrieval uses an exact scan; there is no ANN backend.
- Parsing extracts syntax definitions without cross-file references or call graphs.
- Local embeddings require Ollama; in-process embeddings are not included.
- There is no multi-project aggregation, shared cache, daemon, quantization, or index compression.
- Reranking results are not persisted. Hybrid database retrieval channels run sequentially.
