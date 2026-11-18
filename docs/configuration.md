# 配置

## 来源与优先级

从高到低：

1. 命令行：位置参数路径、`--root`、`--config`、`--limit`、`--mode` 等。
2. 环境变量 `SOLASCOPE__<分组>__<字段>`。
3. 项目配置 `<项目根>/.solascope.toml`（或 `--config` 指定的文件）。
4. 用户全局配置 `<系统配置目录>/solascope/config.toml`（Linux 为 `~/.config/solascope/config.toml`）。
5. 内置默认值。

未知字段和未知的 `SOLASCOPE__*` 变量会被拒绝，避免拼写错误被静默忽略。

配置文件里的相对路径相对该文件所在目录解析；环境变量和命令行里的相对路径相对当前工作目录解析。

### 环境变量写法

分组和字段名大写，用双下划线分隔。字符串和路径直接写值，布尔、数字、数组使用 TOML 语法：

```bash
export SOLASCOPE__EMBEDDING__PROVIDER=ollama
export SOLASCOPE__SEARCH__LIMIT=20
export SOLASCOPE__PROJECT__EXCLUDE='["generated/**"]'
```

## 字段

`solascope init` 生成的 `.solascope.toml` 包含全部字段及默认值。

### project

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `root` | `.` | 项目根目录 |
| `index_dir` | `.solascope` | 索引目录；必须不包含项目根 |
| `include` | `[]` | 非空时只索引匹配这些 glob 的文件 |
| `exclude` | `[]` | 追加排除的 glob |
| `max_file_size` | `1048576` | 超过该字节数的文件跳过 |

### parser

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled_languages` | `rust, python, typescript, javascript, go` | 使用语法树解析的语言 |
| `fallback_policy` | `text` | `text`：其余文件按行分块；`skip`：其余文件不索引 |
| `max_chunk_tokens` | `512` | 单个 Chunk 的预算，按 4 字节每 token 保守估算 |
| `overlap_lines` | `5` | 超长定义拆分时相邻块的重叠行数 |

### embedding

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`、`ollama`、`openai` |
| `model` | `nomic-embed-text` | 模型名 |
| `endpoint` | `http://127.0.0.1:11434` | 服务地址；`openai` 可带或不带 `/v1` |
| `dimensions` | `768` | 向量维度，必须与模型实际输出一致 |
| `api_key_env` | `OPENAI_API_KEY` | 存放 API Key 的环境变量**名称**；`ollama` 不发送密钥 |
| `batch_size` | `32` | 每个请求的文本数 |
| `concurrency` | `2` | 并发请求数 |
| `timeout_secs` | `30` | 单次请求超时 |
| `retries` | `2` | 429、5xx 和临时网络错误的重试次数，最多 5 |

`provider`、`model`、`endpoint`、`dimensions` 构成索引指纹，修改后需要 `solascope index --rebuild`。`batch_size`、`concurrency`、`timeout_secs`、`retries` 不影响指纹。

### rerank

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`、`remote`、`fastembed` |
| `model` | 空 | 启用时必填 |
| `endpoint` | 空 | `remote` 必填；请求发往 `{endpoint}/rerank` |
| `api_key_env` | `RERANK_API_KEY` | 存放 API Key 的环境变量名称 |
| `top_n` | `20` | 请求模型返回的最大结果数 |
| `timeout_secs` | `30` | 单次请求超时 |
| `fail_open` | `true` | 失败时保留融合排序并输出警告；`false` 时搜索报错 |
| `max_input_chars` | `16000` | 每个候选文本的最大字符数 |
| `cache_dir` | 无 | `fastembed` 的离线模型目录 |
| `allow_model_download` | `false` | 是否允许 `fastembed` 首次使用时下载模型 |

### search

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `mode` | `hybrid` | `lexical`、`semantic`、`hybrid` |
| `lexical_top_k` | `40` | 词法通道候选数 |
| `vector_top_k` | `40` | 向量通道候选数 |
| `symbol_top_k` | `20` | 符号通道候选数 |
| `fusion_k` | `60.0` | RRF 常数 |
| `rerank_candidates` | `30` | 送入 Reranker 的候选数 |
| `limit` | `10` | 默认返回条数，最大 100 |
| `semantic_fail_open` | `true` | `hybrid` 模式下 Embedding 不可用时降级为词法加符号 |

### index

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `debounce_ms` | `300` | `watch` 的事件防抖时间 |
| `max_concurrent_jobs` | `4` | 解析线程数 |
| `snapshot_retention` | `2` | 保留的索引代际记录数 |

### privacy

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `remote_opt_in` | `false` | 允许把代码片段发送到远程模型 |
| `sensitive_path_patterns` | `[]` | 追加的敏感路径 glob，命中即不索引、不可读取 |
| `symlink_policy` | `skip` | `skip`：忽略符号链接；`within_root`：跟随目标仍在根内的链接 |
| `allow_external_paths` | `false` | v0.1 不支持设为 `true` |

### log

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `level` | `info` | `off`、`error`、`warn`、`info`、`debug`、`trace`；日志写到 stderr |

## 始终排除的路径

以下名称出现在路径的任意一级时都会被排除，配置无法解除：

- 版本控制与索引：`.git`、`.hg`、`.svn`、`.solascope`、`.solascope.toml`
- 构建与依赖：`target`、`node_modules`、`vendor`、`deps`、`build`、`dist`、`.venv`、`venv`、`__pycache__`、`.mypy_cache`、`.pytest_cache`、`.next`、`coverage`、`.cache`
- 凭据：`.env`、`.env.*`、`.ssh`、`.aws`、`.gnupg`、`.kube`、`secrets`、`credentials`、`.npmrc`、`.pypirc`、`.netrc`、`credentials.json`、`id_rsa`、`id_dsa`、`id_ecdsa`、`id_ed25519`，以及 `.pem`、`.key`、`.p12`、`.pfx`、`.keystore` 后缀

注意：源码目录如果恰好叫 `build`、`dist`、`vendor`、`deps` 或 `coverage`，其中的文件不会被索引。

此外跳过非 UTF-8 文件、二进制文件、超过 `max_file_size` 的文件，以及内容以私钥头开始的文件。

## 示例

### 本机 Ollama

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

Ollama 地址不是本机回环地址时，同样需要 `privacy.remote_opt_in = true`。

### OpenAI 兼容的 Embedding 与远程 Rerank

```toml
[privacy]
remote_opt_in = true

[embedding]
provider = "openai"
model = "text-embedding-3-small"
endpoint = "https://api.openai.com"
dimensions = 1536
api_key_env = "OPENAI_API_KEY"

[rerank]
provider = "remote"
model = "rerank-v3.5"
endpoint = "https://api.cohere.com/v2"
api_key_env = "RERANK_API_KEY"
```

`openai` 和 `remote` 无论地址是否在本机都要求 `remote_opt_in`，并且总是读取 `api_key_env` 指向的环境变量；自建的无鉴权服务可以给该变量设置任意非空值。

模型名、维度和地址以所用服务的文档为准，上面的取值未在本项目中实际连接验证。

### 本地 ONNX 重排

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

`cache_dir` 内需要平铺 `model.onnx`、`tokenizer.json`、`config.json`、`special_tokens_map.json`、`tokenizer_config.json`。未提供离线文件时，只有设置 `allow_model_download = true` 才会下载。该特性目前只通过了编译检查。
