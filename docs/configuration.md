# Configuration

## Sources and precedence

Configuration sources apply in this order, from highest to lowest priority:

1. Command-line options and arguments, including the positional path, `--root`, `--config`, `--limit`, and `--mode`.
2. Existing process environment variables named `SOLASCOPE__<SECTION>__<FIELD>`.
3. The `.env` file beside the project configuration, or beside the file selected by `--config`.
4. Project configuration: `.solascope.toml` in an explicitly selected source directory; otherwise the nearest configuration found from the current directory upward, stopping at a Git root. `--config` selects a specific file.
5. The `.env` file beside the user configuration.
6. User configuration: `~/.solascope/.solascope.toml` on Linux/macOS, or `$env:USERPROFILE\.solascope\.solascope.toml` on Windows (PowerShell syntax).
7. Built-in defaults.

`solascope init` / `sas init` creates only the user configuration described above. It does not write files to the project directory or overwrite an existing configuration. The generated file omits `project.root` and `project.index_dir`, so indexing and search continue to use the current project or `--root`. Project configuration is an optional override for advanced use; `init` no longer accepts a positional project path.

If `init` reports `already exists and was left unchanged`, it refused to overwrite the user configuration and returned exit code 2. This message does not mean the existing configuration is invalid. Skip `init` and run `index` with the existing settings.

If the new user configuration does not exist, SolaScope reads the legacy `<system-config-directory>/solascope/config.toml` for compatibility. It does not move or rewrite that file, or merge the new and legacy user configurations.

Unknown fields and unknown `SOLASCOPE__*` variables are rejected to catch typos. Relative paths declared in TOML and `.env` files are resolved from the directory containing each file. Relative paths in process environment variables and command-line arguments are resolved from the current working directory. `log.output = "stderr"` names a stream and is not resolved as a file path.

### Source directory and configuration location

Directory selection is separate from configuration precedence: the positional path (`ignore [path]` / `index [path]` / `watch [path]`) takes precedence over `--root`, then the process environment variable `SOLASCOPE__PROJECT__ROOT`, then the current working directory. By default, `index` recursively scans the current directory and all levels of subdirectories without expanding to a parent Git root. Ignore rules, file-size limits, and privacy filters still apply.

When no source directory is explicitly selected, a parent configuration can supply settings. A positional path, `--root`, or process environment root limits automatic project configuration loading to that directory; unrelated parent configurations are not inherited. Use `--config` to load settings from another directory.

A `project.root` value in TOML or a sibling `.env` does not change the source directory; the field remains readable for compatibility with existing configurations. `--config /other/settings.toml` loads that file and its sibling `.env` without indexing `/other`. Loading a parent configuration likewise does not expand the scan. Search, status, watch, and the MCP startup default use the same directory selection rules; pass `--root` explicitly to access a parent project's index. MCP tools also accept an optional `root` on each call, overriding that default for the individual request. A tool-selected directory loads its own project configuration and `.env`, unless startup `--config` selects a shared settings file. See [MCP integration](mcp.md) for configuration lifetime and examples.

Unless `project.index_dir` is explicitly configured, index data is stored in `.solascope/` under the selected source directory. That field controls index storage, not the source directory. Explicit relative paths still resolve beside the TOML or `.env` file that declares them.

### Environment variable syntax

Use uppercase section and field names separated by double underscores. Set strings and paths directly; use TOML syntax for booleans, numbers, and arrays:

```bash
export SOLASCOPE__EMBEDDING__PROVIDER=ollama
export SOLASCOPE__SEARCH__LIMIT=20
export SOLASCOPE__PROJECT__EXCLUDE='["generated/**"]'
```

### A `.env` file beside the configuration

The user configuration can use this layout:

```text
~/.solascope/
├── .solascope.toml
└── .env
```

On Windows, the base directory is `$env:USERPROFILE\.solascope\`. For example, `.env` can contain just credentials:

```dotenv
DASHSCOPE_API_KEY=replace-with-your-bailian-key
SILICONFLOW_API_KEY=replace-with-your-siliconflow-key
```

Keep variable names such as `api_key_env = "DASHSCOPE_API_KEY"` in TOML; there is no need to `export` those variables separately. Existing process variables take precedence, including values explicitly set to an empty string. Credentials beside the project configuration can override credentials with the same name in the user directory. You can also put `SOLASCOPE__...` variables in `.env` to override TOML fields according to the precedence above.

The parser supports blank lines, comments, `export NAME=VALUE`, single and double quotes, and multiline values. The last definition of a variable in a file wins; shell commands are not executed. Only `.env` files beside configuration files in use are loaded. SolaScope does not search parent directories or treat a working-directory `.env` as global configuration when there is no project configuration. Each configuration instance loads its own values without modifying the process environment.

A `.env` file must be a regular UTF-8 file no larger than 64 KiB; symlinks are rejected. Missing files are allowed, and syntax errors do not echo secret contents. Neither file contents nor secret values are included in serialized configuration, model fingerprints, or diagnostic logs. The files remain excluded from indexing and `context` reads. On Linux/macOS, use `chmod 600 ~/.solascope/.env`, and do not commit or share the file.

Each CLI invocation reloads these files. Restart MCP/watch services after changing `.env`. Rotating only a key does not require rebuilding the index; changing the embedding model, dimensions, or endpoint does.

## Fields

`solascope init` generates the default user configuration, omitting the project-specific `project.root` and `project.index_dir` values. Embedding and reranking are disabled by default, so no model service is required and no source code is transmitted.

### project

| Field | Default | Description |
| --- | --- | --- |
| `root` | `.` | Runtime source root; TOML / `.env` values do not select the directory. Use a positional path, `--root`, or a process environment variable |
| `index_dir` | `.solascope` | Index storage directory, not the scan scope; must not be the project root or an ancestor of it |
| `include` | `[]` | When nonempty, index only files matching these globs |
| `exclude` | `[]` | Additional exclusion globs |
| `max_file_size` | `1048576` | Skip files larger than this number of bytes |

The index contains source snippets and should be stored privately. When a writer first uses an index directory, it creates a `.gitignore` containing `*` without overwriting an existing file. You must still check whether index files are already tracked by Git.

### Ignore files

Create a project ignore file with either executable:

```bash
solascope ignore
sas ignore --root /path/to/project
solascope ignore /path/to/project --json
```

The command creates `<project-root>/.solascopeignore` with commented examples only; edit it to activate rules. It does not build an index or contact model services. An existing file is left unchanged and returns exit code 2. JSON output contains the created file's absolute `ignore_path`.

Scans and source reads honor `.gitignore` and `.solascopeignore` in each directory, even without a Git repository. Patterns are relative to the directory containing the ignore file. At the same directory level, `.solascopeignore` is applied after `.gitignore`; more nested rules are applied after parent rules.

Use gitignore-style patterns, for example:

```gitignore
# Exclude a directory and its contents.
generated/
# Exclude matching files, except important.log.
*.log
!important.log
```

Blank lines and `#` comments are allowed. Patterns support `*`, `?`, character classes, and `**`; a leading `/` anchors a pattern to the ignore file's directory, a trailing `/` matches directories, and `!` negates an earlier ignore rule. Negation cannot bypass built-in privacy/noise filters, `project.exclude`, `project.include`, the index directory, or sensitive-path restrictions.

Search results and `context` reads follow current ignore rules even before reindexing. Run `index` after editing rules to remove newly ignored files from storage; a running `watch` or `index --watch` process detects ignore-file changes and updates the index automatically.

### parser

| Field | Default | Description |
| --- | --- | --- |
| `enabled_languages` | `rust, python, typescript, javascript, go` | Languages parsed with syntax trees |
| `fallback_policy` | `text` | `text`: split into text chunks when syntax extraction is unavailable; `skip`: do not create fallback text chunks |
| `max_chunk_tokens` | `512` | Budget per chunk, estimated at 4 bytes per token rather than counted with the model's tokenizer |
| `overlap_lines` | `5` | Overlapping lines between adjacent chunks when using line-window fallback; splits at syntax boundaries do not force overlap |

Adjacent comments, attributes, and decorators are kept with their definitions. Fields, constants, and other content not extracted separately from a container are also retained. With `skip`, files in unknown or disabled languages, or files with malformed syntax, produce no chunks and any previously indexed definitions are removed. File records and hashes are still saved for incremental checks, and indexing continues for other files. Files skipped during the current parse pass count toward `skipped_files` and produce a summary warning.

After parser settings or chunking rules change, the next `index` reparses all files automatically and reuses vector cache entries that still match. Read-only queries and `status` return exit code 3 for an index using old chunking rules and ask you to run `index` first. Queries never write to the index automatically.

### embedding

| Field | Default | Description |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`, `ollama`, `openai`, `remote`, `dashscope`, or `siliconflow` |
| `model` | `nomic-embed-text` | Model name; set it according to the service documentation when changing providers |
| `endpoint` | `http://127.0.0.1:11434` | Service root, API base, or full embedding URL; see the protocol table |
| `dimensions` | `768` | Expected vector dimensions; actual output is always validated |
| `request_dimensions` | `true` | Request the configured dimensions when the protocol supports variable dimensions; `false` omits the request field but still validates output dimensions |
| `api_key_env` | `OPENAI_API_KEY` | The **name** of the environment variable containing the API key; `ollama` sends no key |
| `batch_size` | `32` | Configured texts per batch, from 1–256; DashScope reduces batches to known model limits |
| `concurrency` | `2` | Maximum concurrent embedding HTTP requests, from 1–32; requests start with all slots available |
| `timeout_secs` | `30` | Total timeout budget per HTTP batch, from 1–300 seconds, including queueing, shared cooldown, sending, response reads, and retries |
| `retries` | `2` | Retries for retryable rate limits, 5xx, and transient network errors, from 0–5; authentication, balance, and permanent quota errors are not retried |
| `retry_base_delay_ms` | `1000` | Initial fallback retry delay; doubles per attempt and is capped by `retry_max_delay_ms` |
| `retry_max_delay_ms` | `60000` | Maximum fallback retry delay and cooldown, from 1–300000 ms; a provider's `Retry-After` may require a longer wait |
| `rate_limit_cooldown_ms` | `5000` | Minimum configured cooldown after a recognized rate limit; must not exceed `retry_max_delay_ms` |
| `requests_per_minute` | `None` | Optional smooth request budget; when set, spaces actual HTTP attempts at least `60 / value` seconds apart |
| `tokens_per_minute` | `None` | Optional 60-second sliding-window budget based on conservative UTF-8 input byte counts; large batches are split to fit |
| `fingerprint_policy` | `strict` | `strict`: reject incremental writes when the fingerprint changes; `rebuild`: rebuild automatically on the next explicit `index` |

#### Embedding protocols

| Provider | Default appended path | Request and response |
| --- | --- | --- |
| `ollama` | `/api/embed` | `model`, an `input` array, and `truncate: false`; reads `embeddings` |
| `openai`, `remote` | `/v1/embeddings` | OpenAI-compatible format: `model`, `input`, `encoding_format: "float"`, and optional `dimensions`; restores input order using `data[].index` |
| `dashscope` | `/api/v1/services/embeddings/text-embedding/text-embedding` | Native Bailian format: `input.texts` and `parameters.text_type`; sends `output_type: "dense"` for models other than `v1/v2`, with optional singular `dimension`; reads nested `output.embeddings` or top-level `embeddings` |
| `siliconflow` | `/v1/embeddings` | OpenAI-compatible format; omits `dimensions` for fixed-dimension models such as BGE, and can request dimensions for model names containing `Qwen3-Embedding` |

URL construction preserves overlapping path segments such as an existing `/v1`; full endpoint URLs are not appended again. DashScope's `index`, or the legacy `text_index`, restores input order. Duplicate indices, missing vectors, incorrect counts or dimensions, NaN/Inf, and zero vectors are rejected. All vectors are L2-normalized.

DashScope uses `text_type = "document"` for document vectors and `"query"` for search queries. Known batch limits are 10 for `text-embedding-v3/v4`, 25 for `v1/v2`, and 20 for `qwen3.7-text-embedding*`; other models use a conservative limit of 10 texts per batch. `v1/v2` requests omit `dimension` and `output_type`, and each text is checked against the 2,048-character limit before sending. Other models remain subject to the service's token and dimension limits. SolaScope does not silently truncate input or truncate or pad returned vectors.

Indexing queues at most 128 inputs at a time, aligned to whole effective provider batches where they fit within that window. For DashScope's 20-input batches, intermediate groups contain 120 inputs, so only the final group may need a partial batch. Completed HTTP batches immediately refill available concurrency slots, even when an earlier request is slow. Results are restored to input order before vectors are associated with source chunks.

`embedding.concurrency` is a maximum, and all slots are initially available. A recognized HTTP 429 or known rate-limit code in an HTTP 200 error envelope places every embedding request using that provider instance into a shared cooldown. Engine clones share their adapter and cooldown; separate provider or Engine instances and separate processes do not. A 429 sets effective concurrency to one. Successful requests already in flight from an earlier cooldown epoch do not count toward recovery. Every three consecutive successes in the new epoch increase effective concurrency by one, up to the configured maximum. Every ten consecutive successes reduce the base cooldown for the next rate-limit event, down to `rate_limit_cooldown_ms`.

Fallback retry delay is `retry_base_delay_ms × 2^attempt`, capped at `retry_max_delay_ms`, with nonnegative bounded jitter that never exceeds the cap. A recognized 429 starts at the configured cooldown and doubles it on later rate-limit events up to the maximum. Concurrent requests observing the same cooldown event do not double it again, though a longer `Retry-After` can extend the wait. Both delta-seconds and HTTP-date `Retry-After` values are supported; a valid hint is a minimum wait and is not shortened to the fallback maximum. Authentication, balance, and permanent quota failures are not retried. Reranking retains its existing independent retry behavior; embedding retry budgets do not apply to rerank requests.

When `requests_per_minute` is set, actual HTTP attempts are spaced smoothly by at least `60 / requests_per_minute` seconds. When `tokens_per_minute` is set, a 60-second sliding window tracks conservative UTF-8 input byte counts. This is an input-size estimate, not the provider's tokenizer or reported billed usage, so actual provider limits may still be reached. Large text batches are split to fit the full minute budget, then wait for sufficient sliding-window capacity. A single text larger than the full minute budget fails before sending and asks the user to increase the configured budget. Requests are not charged before they leave the queue; every actual HTTP attempt, including failures and retries, consumes budget.

`timeout_secs` bounds the complete lifetime of each HTTP batch, including queueing, cooldown waits, HTTP sending, response-body reads, and all retries. When the budget expires, the request future is canceled and queue or active concurrency capacity is released. The `retries` setting still limits the number of retries independently.

The `requesting embeddings` event reports `effective_batch_size`, configured `concurrency`, and `group_inputs`. Progress events include `batch_elapsed_ms` for the group and average `inputs_per_second` for the embedding phase. With `--verbose`, `embedding batch complete` events report individual HTTP batch input counts and elapsed milliseconds. Rate-limit logs contain only numeric durations, effective concurrency, and classified errors; they do not record source text, keys, URLs, or raw HTTP headers.

Use `request_dimensions = false` for compatible services that do not accept variable-dimension parameters. In that case, `dimensions` must match the model's default output dimensions.

#### Fingerprints and cache reuse after moves

The fingerprint binds the provider, model, normalized full endpoint, dimensions, normalization, input template, and dimension-request policy. After changing a setting that affects the vector space, the default policy requires:

```bash
solascope index --rebuild
```

With `fingerprint_policy = "rebuild"`, the next explicit indexing operation rebuilds automatically and emits a warning. Queries always reject an incompatible model/index combination and never rebuild automatically. Batch size, concurrency, timeout, retry delays, cooldown, RPM/TPM budgets, retries, and the key's environment variable name are not part of the vector-space fingerprint. The source template and vector fingerprint remain unchanged by these rate-limit settings.

Moving code within a file reuses the exact cache entry when the complete embedding input stays the same. Moving or renaming a file can reuse vectors from the old path when the language, symbol, signature, and body match. This is approximate reuse because the template includes the path, and it is counted in `relocated_embeddings` / `relocated_vectors`. An explicit `--rebuild` recomputes these vectors using the current path while reusing other exact cache entries.

### rerank

| Field | Default | Description |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`, `remote`, `openai`, `dashscope`, `siliconflow`, or `fastembed` |
| `model` | Empty | Required when enabled |
| `endpoint` | Empty | Required for HTTP providers; accepts a full `/rerank` or `/reranks` URL |
| `api_key_env` | `RERANK_API_KEY` | Name of the environment variable containing the API key |
| `top_n` | `20` | Maximum results requested from the model, capped at the actual candidate count |
| `timeout_secs` | `30` | Timeout for an HTTP request or local call |
| `fail_open` | `true` | Preserve the fused ranking and emit a warning on failure; `false` makes the search fail |
| `max_input_chars` | `16000` | Character limit per candidate text, also subject to the provider's declared limit |
| `cache_dir` | None | Offline model directory for `fastembed` |
| `allow_model_download` | `false` | Allow `fastembed` to download a model on first use |

#### Reranking protocols

| Provider | Default appended path | Request and response |
| --- | --- | --- |
| `remote` | `/rerank` | Generic flat `model/query/documents/top_n/return_documents` fields; top-level `results[].index/relevance_score` |
| `openai` | `/v1/rerank` | Same as the generic format; a Bailian `/compatible-api/v1` base uses the plural `/reranks` |
| `dashscope` | `/api/v1/services/rerank/text-rerank/text-rerank` | Native `input.query/documents` and `parameters.top_n/return_documents`; nested `output.results` |
| `siliconflow` | `/v1/rerank` | Generic flat format and top-level `results` |
| `fastembed` | No HTTP | In-process ONNX cross-encoder; requires the `local-rerank` build feature |

Use `openai` for reranking services that implement the compatible HTTP format above; consult the provider's documentation for its exact protocol. For services using the plural path, configure a full URL ending in `/reranks`. Native DashScope and compatible mode use different response structures, so select the corresponding provider. Only text documents are currently accepted.

SolaScope preserves each candidate's original index and ID, and rejects duplicate or out-of-range indices and non-finite scores. Candidate counts and character counts are limited by both configuration and provider limits. Before inference, local models also validate that the complete query/document pair fits within 512 tokens. A character budget cannot guarantee a token limit for every tokenizer. A running ONNX call cannot be forcibly interrupted after a timeout, but it continues to hold its concurrency permit to prevent unbounded queuing.

Embedding and reranking providers can be combined independently. Changing the reranker does not require rebuilding the index. Caching covers adapter/local model instances and query vectors; reranking results are not persisted in a cache.

### search

| Field | Default | Description |
| --- | --- | --- |
| `mode` | `hybrid` | `lexical`, `semantic`, or `hybrid` |
| `lexical_top_k` | `40` | Candidates from the lexical channel |
| `vector_top_k` | `40` | Candidates from the vector channel |
| `symbol_top_k` | `20` | Candidates from the symbol channel |
| `fusion_k` | `60.0` | RRF constant |
| `rerank_candidates` | `30` | Candidates sent to the reranker |
| `limit` | `10` | Default result count, up to 100 |
| `semantic_fail_open` | `true` | Fall back to lexical plus symbol retrieval in `hybrid` mode when the model is temporarily unavailable or authentication fails |

`lexical` retrieves only from FTS5; `semantic` retrieves only from vectors. Only `hybrid` adds the independent symbol channel. Reranking can be enabled independently for any of the three modes. Model incompatibility and dimension errors never trigger silent fallback, and `semantic` mode does not fall back. The `symbol` command also supports limited typo correction and marks those results with `fuzzy`; hybrid search does not include these fuzzy candidates. Search JSON includes `embedding_ms`, `retrieval_ms`, `rerank_ms`, and `total_ms` in `timings`. Model stages that hit the cache or are unused report 0.

### index

| Field | Default | Description |
| --- | --- | --- |
| `watch` | `false` | Continue watching after the initial `index` scan; overridden by `--no-watch` |
| `debounce_ms` | `300` | Debounce interval for file events |
| `hash_algorithm` | `sha256` | Only `sha256` is currently supported; other values are rejected |
| `max_concurrent_jobs` | `4` | Parser threads |
| `snapshot_retention` | `2` | Index generation records retained; these are not full historical source snapshots |

`index --watch` and `index --no-watch` are mutually exclusive. `index --watch --json` writes only the initial indexing report to stdout; ongoing updates are logged. The `watch` command always watches, regardless of `index.watch`.

### privacy

| Field | Default | Description |
| --- | --- | --- |
| `remote_opt_in` | `false` | Allow sending code snippets to remote models |
| `sensitive_path_patterns` | `[]` | Additional sensitive-path globs; matching paths cannot be indexed or read |
| `symlink_policy` | `skip` | `skip`: ignore symlinks; `within_root`: follow links whose targets remain inside the root |
| `allow_external_paths` | `false` | Setting this to `true` is unsupported in v0.1; source reads are always confined to the project root |

Every HTTP model provider except Ollama on a local loopback address requires explicit opt-in, even when configured with a local address. Endpoints accept only HTTP(S) and reject embedded credentials, query strings, and fragments. The HTTP client does not follow redirects. Local loopback requests bypass environment proxies, and `localhost` is pinned to `127.0.0.1`.

HTTP errors and error envelopes within HTTP 200 responses are classified by provider error codes. Authentication, balance, and permanent quota failures are not retried; known rate limits and transient failures receive bounded retries and may trigger shared embedding cooldown. Full error bodies, URLs, raw HTTP headers, and keys are not echoed.

### log

| Field | Default | Description |
| --- | --- | --- |
| `level` | `info` | `off`, `error`, `warn`, `info`, `debug`, or `trace` |
| `output` | `stderr` | Console diagnostics to stderr with an automatic file copy, or an explicit log file within the project root |
| `persist` | `true` | With `output = "stderr"`, append a file copy beside the active configuration; `false` keeps stderr only |
| `redact_secrets` | `true` | Additionally redact generic Bearer tokens; configured API key values are always redacted |

By default, CLI diagnostics go to stderr and `<configuration-directory>/logs/solascope.log`. A project configuration or `--config FILE` selects that file's directory; otherwise the user configuration directory is used, even before its TOML file has been created. For the standard user configuration this is `~/.solascope/logs/solascope.log` on Linux/macOS, or `$env:USERPROFILE\.solascope\logs\solascope.log` on Windows. Configuration selection does not change the source directory.

The file is appended across commands. Set `log.persist = false` to disable the automatic copy. An explicit `log.output` file preserves the existing project-confined behavior and is used regardless of `persist`; it replaces the normal trace destinations. Logs cannot escape the selected configuration directory or project root through symlinks, and cannot be written into `.git`.

File logs carry the marker `# SolaScope diagnostic log`. SolaScope appends only to logs it created; an existing file without that marker is left unchanged and causes an error. New Unix files use permissions `0600`. The selected log path is excluded from indexing and source-context reads, including when automatic persistence is disabled.

HTTP diagnostics record retry attempts, status codes, wait times, and static error categories. Final CLI failures are also logged. Request URLs, request/response bodies, vendor error messages, and credentials are excluded from model-transport events. All destinations use the same secret-redaction path.

`--verbose` sets SolaScope modules to debug; third-party modules remain at warn by default to avoid implicitly logging model/MCP bodies. Without `--verbose`, an explicit `RUST_LOG` can override levels. Do not enable verbose logging for protocol libraries that may print complete requests or responses. File logging changes only the diagnostic log destination; CLI errors, warnings, and MCP probe messages still stay out of stdout.

#### Investigating HTTP 429

A 429 generally indicates a provider rate limit. Check the model's account-specific request and token quotas; some services also use it for quota errors. The diagnostic event distinguishes recognized authentication/quota codes from retryable rate limits without recording the vendor's error body. An unrecognized code remains classified by HTTP status.

Provider-specific batch caps can split one indexing progress group into several HTTP requests. `embedding.concurrency` is a maximum and starts fully available; shared rate-limit cooldown and adaptive recovery temporarily adjust effective concurrency. Configure `requests_per_minute` and `tokens_per_minute` for provider budgets. Without a valid `Retry-After`, retries use the configured exponential fallback delay and bounded nonnegative jitter. A valid, representable `Retry-After` is honored as a minimum wait, even when longer than the fallback delay cap. An unrepresentable rate-limit hint fails that call without retrying and leaves a finite fallback cooldown instead of permanently blocking the adapter.

RPM/TPM budgets are unset by default because account quotas vary. Use limits appropriate for your provider and leave headroom for other clients. With minute-window budgets, increase `timeout_secs` when necessary (for example, to `120`): its default 30-second total budget may expire while waiting for the next window. The maximum remains 300 seconds.

Index progress is not a persistent checkpoint. New embeddings are published with the complete index transaction; a failed run leaves the existing index unchanged and can require those uncommitted embeddings again on retry. Changing batch size, concurrency, or retries does not change the embedding fingerprint and does not require `--rebuild`.

## Always-excluded paths

The following names are excluded at every path level, case-insensitively. Configuration cannot override these exclusions:

- Version control and index files: `.git`, `.hg`, `.svn`, `.solascope`, `.solascope.toml`
- Build outputs and dependencies: `target`, `node_modules`, `vendor`, `deps`, `build`, `dist`, `.venv`, `venv`, `__pycache__`, `.mypy_cache`, `.pytest_cache`, `.next`, `coverage`, `.cache`
- Credentials: `.env`, `.env.*`, `.ssh`, `.aws`, `.gnupg`, `.kube`, `secrets`, `credentials`, `.npmrc`, `.pypirc`, `.netrc`, `credentials.json`, `id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519`, and the suffixes `.pem`, `.key`, `.p12`, `.pfx`, `.keystore`

Files in source directories named `build`, `dist`, `vendor`, `deps`, or `coverage` will not be indexed. Non-UTF-8 files, binary files, files larger than `max_file_size`, and files containing detected private-key material are also skipped.

## Examples

The cloud model names, dimensions, and endpoints below are configuration examples and have not been verified against live services in this project. Adjust them for your account's region, workspace, and model documentation. Set keys only through the corresponding environment variables, never in TOML.

### Local Ollama

```toml
[embedding]
provider = "ollama"
model = "nomic-embed-text"
endpoint = "http://127.0.0.1:11434"
dimensions = 768
```

```bash
ollama pull nomic-embed-text
solascope index --rebuild
solascope doctor --models
```

Install Ollama and prepare the model yourself; SolaScope does not install them automatically. An Ollama endpoint outside local loopback also requires `privacy.remote_opt_in = true`.

### Generic/OpenAI-compatible APIs

```toml
[privacy]
remote_opt_in = true

[embedding]
provider = "openai"
model = "text-embedding-3-small"
endpoint = "https://api.openai.com/v1"
dimensions = 1536
api_key_env = "OPENAI_API_KEY"

[rerank]
provider = "remote"
model = "rerank-v3.5"
endpoint = "https://api.cohere.com/v2/rerank"
api_key_env = "RERANK_API_KEY"
```

For embedding, `remote` and `openai` use the same protocol. For reranking, `openai` works with providers that support the flat compatible request format. Self-hosted compatible services without authentication still require the corresponding key environment variable to have a nonempty value.

### Native Bailian DashScope APIs

`YOUR_WORKSPACE_ID` is a placeholder; replace it with the workspace domain from your console and select the region for your account.

```toml
[privacy]
remote_opt_in = true

[embedding]
provider = "dashscope"
model = "text-embedding-v4"
endpoint = "https://YOUR_WORKSPACE_ID.cn-beijing.maas.aliyuncs.com"
dimensions = 1024
batch_size = 10
api_key_env = "DASHSCOPE_API_KEY"

[rerank]
provider = "dashscope"
model = "qwen3-rerank"
endpoint = "https://YOUR_WORKSPACE_ID.cn-beijing.maas.aliyuncs.com"
api_key_env = "DASHSCOPE_API_KEY"
```

If your workspace exposes a compatible API instead of the native API, use `provider = "openai"` and a `/compatible-api/v1` base for embedding. For compatible reranking, use:

```toml
[rerank]
provider = "openai"
model = "qwen3-rerank"
endpoint = "https://YOUR_WORKSPACE_ID.cn-beijing.maas.aliyuncs.com/compatible-api/v1/reranks"
api_key_env = "DASHSCOPE_API_KEY"
```

Use the service endpoints shown in the current Bailian console and documentation. Do not assume endpoints from different workspaces or legacy domains are interchangeable.

### SiliconFlow

```toml
[privacy]
remote_opt_in = true

[embedding]
provider = "siliconflow"
model = "BAAI/bge-m3"
endpoint = "https://api.siliconflow.cn/v1"
dimensions = 1024
api_key_env = "SILICONFLOW_API_KEY"

[rerank]
provider = "siliconflow"
model = "BAAI/bge-reranker-v2-m3"
endpoint = "https://api.siliconflow.cn/v1"
api_key_env = "SILICONFLOW_API_KEY"
```

This fixed-dimension embedding example does not send a `dimensions` parameter. For variable-dimension Qwen3 models, use model names and dimensions supported by the service.

### Local ONNX reranking

```bash
cargo build --release --features local-rerank
export ORT_DYLIB_PATH=/path/to/libonnxruntime.so
```

```toml
[rerank]
provider = "fastembed"
model = "BAAI/bge-reranker-base"
cache_dir = "/path/to/reranker-bundle"
```

The five required files—`model.onnx`, `tokenizer.json`, `config.json`, `special_tokens_map.json`, and `tokenizer_config.json`—must be at the top level of `cache_dir`. Without offline files, downloads are allowed only when `allow_model_download = true`. You must provide a compatible ONNX Runtime. Real model loading and inference have not been verified; compilation and mock tests do not establish quality improvements.

## Probes and diagnostics

`init --probe`, `index --probe`, `watch --probe`, `mcp --probe`, and `doctor --models` use only fixed test sentences, never source code, as probe input. They still call the configured models and may incur charges. Default startup does not implicitly probe services, download models, or trigger a rebuild.

`init --probe --config FILE` checks the models in the supplied configuration; the generated user configuration still uses defaults with models disabled. `mcp --probe` writes probe information only to stderr.

`doctor` reports configuration, database, and model checks. A missing index or one requiring an upgrade produces a warning; detected database errors return exit code 1, and model errors return 4. Ordinary read-only queries return 3 for an old schema or old chunking rules. `doctor --prune-cache` requires the writer lock and removes only unreferenced cache entries.

## Provider documentation

- [Bailian synchronous embedding API](https://docs.bailian.console.aliyun.com/zh/model-studio/text-embedding-synchronous-api)
- [Bailian text reranking API](https://docs.bailian.console.aliyun.com/zh/model-studio/text-rerank-api)
- [SiliconFlow embeddings API](https://api-docs.siliconflow.cn/docs/api/embeddings-post)
- [SiliconFlow reranking API](https://api-docs.siliconflow.cn/docs/api/rerank-post)

See [Support and limitations](implementation-status.md) for the current verification scope.
