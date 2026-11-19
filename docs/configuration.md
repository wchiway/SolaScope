# 配置

## 来源与优先级

从高到低：

1. 命令行：位置参数路径、`--root`、`--config`、`--limit`、`--mode` 等。
2. 已有进程环境变量 `SOLASCOPE__<分组>__<字段>`。
3. 项目配置（或 `--config` 指定的配置）同目录的 `.env`。
4. 项目配置 `<项目根>/.solascope.toml`（或 `--config` 指定的文件）。
5. 用户全局配置同目录的 `.env`。
6. 用户全局配置：Linux/macOS 为 `~/.solascope/.solascope.toml`，Windows 为 `$USERPROFILE/.solascope/.solascope.toml`。
7. 内置默认值。

`solascope init` / `sas init` 只创建上述用户配置，不在项目目录写入文件，不覆盖已有配置。生成的用户配置不设置 `project.root` 或 `project.index_dir`，所以索引和搜索仍按当前项目或 `--root` 工作。项目配置是可选的高级覆盖；`init` 不再接受项目路径位置参数。

新用户配置不存在时，兼容读取旧的 `<系统配置目录>/solascope/config.toml`；不会自动移动、改写旧文件，也不会同时叠加新旧两份用户配置。

未知字段和未知的 `SOLASCOPE__*` 变量会被拒绝，避免拼写错误被静默忽略。TOML 和 `.env` 中声明的相对路径相对各自文件所在目录解析；进程环境变量和命令行里的相对路径相对当前工作目录解析。`log.output = "stderr"` 是流名称，不按文件路径解析。

### 环境变量写法

分组和字段名大写，用双下划线分隔。字符串和路径直接写值，布尔、数字、数组使用 TOML 语法：

```bash
export SOLASCOPE__EMBEDDING__PROVIDER=ollama
export SOLASCOPE__SEARCH__LIMIT=20
export SOLASCOPE__PROJECT__EXCLUDE='["generated/**"]'
```

### 同目录 `.env`

用户级配置可采用以下布局：

```text
~/.solascope/
├── .solascope.toml
└── .env
```

Windows 的基础目录是 `$USERPROFILE/.solascope/`。例如 `.env` 中只写凭据：

```dotenv
DASHSCOPE_API_KEY=replace-with-your-bailian-key
SILICONFLOW_API_KEY=replace-with-your-siliconflow-key
```

TOML 仍使用 `api_key_env = "DASHSCOPE_API_KEY"` 等变量名，不需要再 `export` 同名变量。已有进程变量优先，包括显式设置为空的值；项目配置同目录的凭据可覆盖用户目录的同名凭据。`SOLASCOPE__...` 变量也可以写在 `.env` 中，按上面的优先级覆盖 TOML 字段。

支持空行、注释、`export NAME=VALUE`、单/双引号和多行值；同文件重名变量最后一项生效，不执行 shell 命令。仅加载实际配置对应目录的 `.env`，不会沿父目录搜索，也不会把无项目配置的工作目录 `.env` 当作全局配置。加载采用配置实例隔离，不修改整个进程的环境变量。

`.env` 必须是 UTF-8 普通文件，最多 64 KiB，拒绝符号链接；不存在时不报错，语法错误不会回显密钥内容。文件、密钥值不会进入配置序列化、模型指纹或诊断日志，仍禁止索引和 `context` 读取。Linux/macOS 建议设置 `chmod 600 ~/.solascope/.env`，不要提交或分享该文件。

每次 CLI 启动重新读取；修改 `.env` 后应重启 MCP/watch 服务。只轮换密钥不需要重建索引；改变 Embedding 模型、维度或地址仍需重建。

## 字段

`solascope init` 生成用户默认配置，省略需要按项目确定的 `project.root` / `project.index_dir`。默认 Embedding 和 Reranker 均禁用，不需要模型服务，也不传输源码。

### project

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `root` | `.` | 项目根目录 |
| `index_dir` | `.solascope` | 索引目录；不能等于项目根或成为项目根的祖先 |
| `include` | `[]` | 非空时只索引匹配这些 glob 的文件 |
| `exclude` | `[]` | 追加排除的 glob |
| `max_file_size` | `1048576` | 超过该字节数的文件跳过 |

索引包含源码片段，应保存在私有位置。写入者首次使用索引目录时会创建内容为 `*` 的 `.gitignore`，不覆盖已有文件；这不能代替检查已被 Git 跟踪的索引文件。

### parser

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled_languages` | `rust, python, typescript, javascript, go` | 使用语法树解析的语言 |
| `fallback_policy` | `text` | `text`：无法使用语法提取时按文本分块；`skip`：不生成文本回退块 |
| `max_chunk_tokens` | `512` | 单个 Chunk 的预算，按 4 字节每 token 估算，并非实际模型 tokenizer 计数 |
| `overlap_lines` | `5` | 回退到行窗口时相邻块的重叠行数；语法边界拆分不强制重叠 |

相邻注释、属性、装饰器随定义保存；容器中未单独提取的字段、常量等也保留。设为 `skip` 时，未知/禁用语言或损坏语法的文件不生成 Chunk，原有定义会移除，但仍保存文件记录和哈希用于增量检查，不会中止其他文件的索引。当轮解析中跳过的文件计入 `skipped_files` 并输出汇总警告。

修改解析器设置或升级分块规则后，下次 `index` 自动重新解析全部文件，并复用仍匹配的向量缓存。旧分块规则的只读查询和 `status` 返回退出码 3，提示先运行 `index`；查询不会自动写入索引。

### embedding

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`、`ollama`、`openai`、`remote`、`dashscope`、`siliconflow` |
| `model` | `nomic-embed-text` | 模型名；更换 provider 时需按服务文档设置 |
| `endpoint` | `http://127.0.0.1:11434` | 服务根地址、API base 或完整 Embedding URL，见协议表 |
| `dimensions` | `768` | 预期向量维度，始终校验实际输出 |
| `request_dimensions` | `true` | 支持可变维度的协议是否请求指定维度；`false` 时省略请求字段，但仍校验输出维度 |
| `api_key_env` | `OPENAI_API_KEY` | 存放 API Key 的环境变量**名称**；`ollama` 不发送密钥 |
| `batch_size` | `32` | 配置的每批文本数，范围 1–256；DashScope 按已知模型上限缩小批次 |
| `concurrency` | `2` | 并发请求数，范围 1–32 |
| `timeout_secs` | `30` | 单次 HTTP 请求超时 |
| `retries` | `2` | 429、5xx 和临时网络错误的重试次数，最多 5 |
| `fingerprint_policy` | `strict` | `strict`：指纹改变时拒绝增量写入；`rebuild`：下次显式 `index` 自动重建 |

#### Embedding 协议

| provider | 默认补全的路径 | 请求与响应 |
| --- | --- | --- |
| `ollama` | `/api/embed` | `model`、`input` 数组、`truncate: false`；读取 `embeddings` |
| `openai`、`remote` | `/v1/embeddings` | OpenAI 兼容格式：`model`、`input`、`encoding_format: "float"`，可选 `dimensions`；按 `data[].index` 恢复输入顺序 |
| `dashscope` | `/api/v1/services/embeddings/text-embedding/text-embedding` | 百炼原生格式：`input.texts`、`parameters.text_type`；非 `v1/v2` 模型发送 `output_type: "dense"`，可选单数 `dimension`；读取嵌套 `output.embeddings` 或顶层 `embeddings` |
| `siliconflow` | `/v1/embeddings` | OpenAI 兼容格式；固定维度 BGE 等模型省略 `dimensions`，名称含 `Qwen3-Embedding` 的模型可请求维度 |

URL 拼接保留已有的 `/v1` 等重叠路径，完整接口 URL 不重复追加。DashScope 返回的 `index` 或旧格式 `text_index` 用于恢复输入顺序；重复索引、缺失向量、数量或维度错误、NaN/Inf、零向量均拒绝使用。向量统一进行 L2 归一化。

DashScope 文档向量使用 `text_type = "document"`，搜索查询使用 `"query"`。已知批次上限为 `text-embedding-v3/v4` 10、`v1/v2` 25、`qwen3.7-text-embedding*` 20；其他模型保守限制为每批 10。`v1/v2` 不发送 `dimension` 或 `output_type`，并在请求前检查每条文本最多 2048 字符。其他模型仍受服务自身 token/维度限制约束，程序不静默截断输入，也不截断或补齐返回向量。

`request_dimensions = false` 适用于不接受可变维度参数的兼容服务，此时 `dimensions` 必须与模型默认输出维度一致。

#### 指纹与移动缓存

指纹绑定 provider、模型、规范化后的完整 endpoint、维度、归一化和输入模板/维度请求策略。修改向量空间相关项后，默认需运行：

```bash
solascope index --rebuild
```

设为 `fingerprint_policy = "rebuild"` 时，下一次显式索引会自动重建并输出警告；查询始终拒绝模型与索引不兼容，不会自动重建。批大小、并发、超时、重试、密钥环境变量名不属于向量空间指纹。

同一文件内移动代码且 Embedding 完整输入不变时精确复用缓存。移动或重命名文件可按相同语言、符号、签名、正文复用旧路径向量；这属于近似复用，因为模板包含路径，计入 `relocated_embeddings` / `relocated_vectors`。显式 `--rebuild` 会按当前路径重新计算这些向量，仍复用其余精确缓存。

### rerank

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `provider` | `disabled` | `disabled`、`remote`、`openai`、`dashscope`、`siliconflow`、`fastembed` |
| `model` | 空 | 启用时必填 |
| `endpoint` | 空 | HTTP provider 必填；可使用完整 `/rerank` 或 `/reranks` URL |
| `api_key_env` | `RERANK_API_KEY` | 存放 API Key 的环境变量名称 |
| `top_n` | `20` | 请求模型返回的最大结果数，不超过实际候选数 |
| `timeout_secs` | `30` | HTTP 请求或本地调用的超时 |
| `fail_open` | `true` | 失败时保留融合排序并输出警告；`false` 时搜索报错 |
| `max_input_chars` | `16000` | 每个候选文本的字符上限，还受 provider 声明的限制约束 |
| `cache_dir` | 无 | `fastembed` 的离线模型目录 |
| `allow_model_download` | `false` | 是否允许 `fastembed` 首次使用时下载模型 |

#### Rerank 协议

| provider | 默认补全的路径 | 请求与响应 |
| --- | --- | --- |
| `remote` | `/rerank` | 通用平铺 `model/query/documents/top_n/return_documents`，顶层 `results[].index/relevance_score` |
| `openai` | `/v1/rerank` | 与通用格式相同；百炼 `/compatible-api/v1` base 改用复数 `/reranks` |
| `dashscope` | `/api/v1/services/rerank/text-rerank/text-rerank` | 原生 `input.query/documents`、`parameters.top_n/return_documents`，嵌套 `output.results` |
| `siliconflow` | `/v1/rerank` | 通用平铺格式和顶层 `results` |
| `fastembed` | 不使用 HTTP | 进程内 ONNX 交叉编码器，需 `local-rerank` 构建特性 |

`openai` 用于采用上述兼容 HTTP 格式的重排服务，具体协议以供应商文档为准。服务使用复数路径时，直接配置以 `/reranks` 结尾的完整 URL。原生 DashScope 与兼容模式的响应容器不能混用。当前仅支持文本文档，不支持多模态 Rerank 输入。

程序保留候选的原始索引和 ID，拒绝重复、越界或非有限评分。候选数和字符数受配置及 provider 上限共同限制；本地模型还在推理前验证完整查询/文档对不超过 512 tokens。字符预算不能保证任意 tokenizer 的 token 上限。超时后正在运行的 ONNX 调用不能被强制抢占，但并发许可持续占用，避免无限排队。

Embedding 与 Reranker 可以独立组合，修改 Reranker 不需要重建索引。缓存策略是复用适配器/本地模型实例及查询向量缓存，不包含持久化的 Rerank 结果缓存。

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
| `semantic_fail_open` | `true` | `hybrid` 模式下模型临时不可用或认证失败时降级为词法加符号 |

`lexical` 只从 FTS5 召回，`semantic` 只从向量召回，`hybrid` 才加入独立符号通道；三种模式可独立选择是否重排。模型不兼容、维度错误不会被静默降级；`semantic` 模式不降级。`symbol` 命令另支持有限拼写纠错，结果以 `fuzzy` 标记，混合搜索不引入这些模糊候选。搜索 JSON 的 `timings` 给出 `embedding_ms`、`retrieval_ms`、`rerank_ms`、`total_ms`；缓存命中或未使用的模型阶段为 0。

### index

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `watch` | `false` | `index` 首次扫描后持续监听；`--no-watch` 可覆盖 |
| `debounce_ms` | `300` | 文件事件防抖时间 |
| `hash_algorithm` | `sha256` | 当前只支持 `sha256`，其他值拒绝 |
| `max_concurrent_jobs` | `4` | 解析线程数 |
| `snapshot_retention` | `2` | 保留的索引代际记录数，不是完整历史源码快照 |

`index --watch` 和 `index --no-watch` 互斥。`index --watch --json` 只在 stdout 输出首次索引报告；持续更新写日志。`watch` 命令始终监听，与 `index.watch` 无关。

### privacy

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `remote_opt_in` | `false` | 允许把代码片段发送到远程模型 |
| `sensitive_path_patterns` | `[]` | 追加的敏感路径 glob，命中即不索引、不可读取 |
| `symlink_policy` | `skip` | `skip`：忽略符号链接；`within_root`：跟随目标仍在根内的链接 |
| `allow_external_paths` | `false` | v0.1 不支持设为 `true`，源文件读取始终受项目根约束 |

所有 HTTP 模型 provider 除本机回环 Ollama 外，都要求显式 opt-in，即使配置了本机地址。endpoint 只接受 HTTP(S)，拒绝内嵌凭据、查询串和 fragment；HTTP 客户端不跟随重定向。本机回环请求不走环境代理，`localhost` 解析固定到本机回环地址。

HTTP 错误和 HTTP 200 内的错误信封按供应商错误码分类：认证/额度失败不重试，已知限流/临时故障有限重试，模型/维度不兼容明确报错。不回显完整错误正文、URL 中的凭据或密钥。

### log

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `level` | `info` | `off`、`error`、`warn`、`info`、`debug`、`trace` |
| `output` | `stderr` | `stderr` 或项目根内的日志文件路径，不能写入 `.git` 或通过符号链接越界 |
| `redact_secrets` | `true` | 额外隐藏通用 Bearer token；配置的 API Key 值始终脱敏 |

文件日志以 `# SolaScope diagnostic log` 标识；只追加本程序创建的日志，已有无此标识的文件保持不变并报错。新建 Unix 文件权限为 `0600`。配置的日志文件不会进入索引或被源码上下文读取。

`--verbose` 将 SolaScope 模块设为 debug；默认第三方模块仍为 warn，避免隐式输出模型/MCP 正文。未指定 `--verbose` 时，显式 `RUST_LOG` 可覆盖级别；不要对协议库开启可能输出完整请求/响应的详细日志。文件日志只影响诊断日志目的地，CLI 错误、警告和 MCP 探测信息仍不进入 stdout。

## 始终排除的路径

以下名称出现在路径的任意一级时都会被排除，配置无法解除（匹配不区分大小写）：

- 版本控制与索引：`.git`、`.hg`、`.svn`、`.solascope`、`.solascope.toml`
- 构建与依赖：`target`、`node_modules`、`vendor`、`deps`、`build`、`dist`、`.venv`、`venv`、`__pycache__`、`.mypy_cache`、`.pytest_cache`、`.next`、`coverage`、`.cache`
- 凭据：`.env`、`.env.*`、`.ssh`、`.aws`、`.gnupg`、`.kube`、`secrets`、`credentials`、`.npmrc`、`.pypirc`、`.netrc`、`credentials.json`、`id_rsa`、`id_dsa`、`id_ecdsa`、`id_ed25519`，以及 `.pem`、`.key`、`.p12`、`.pfx`、`.keystore` 后缀

源码目录如果恰好叫 `build`、`dist`、`vendor`、`deps` 或 `coverage`，其中的文件不会被索引。此外跳过非 UTF-8 文件、二进制文件、超过 `max_file_size` 的文件，以及检测到私钥正文的文件。

## 示例

以下云端模型名、维度和地址是配置示例，未在本项目中实际连接验证。请按账号所在地域、工作空间和模型文档替换；密钥只在对应环境变量中设置，不写入 TOML。

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

需要自行准备 Ollama 和模型，SolaScope 不自动安装。Ollama 地址不是本机回环地址时，也需要 `privacy.remote_opt_in = true`。

### 通用/OpenAI 兼容接口

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

Embedding 的 `remote` 与 `openai` 使用相同协议；Rerank 的 `openai` 可用于支持平铺兼容请求的供应商。无鉴权自建兼容服务仍需把对应密钥环境变量设为非空值。

### 百炼 DashScope 原生接口

`YOUR_WORKSPACE_ID` 是占位符，需替换为控制台给出的工作空间域名；地域也应按实际账号选择。

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

若工作空间提供的是兼容接口而非原生接口，Embedding 改用 `provider = "openai"` 和 `/compatible-api/v1` base。兼容重排则使用：

```toml
[rerank]
provider = "openai"
model = "qwen3-rerank"
endpoint = "https://YOUR_WORKSPACE_ID.cn-beijing.maas.aliyuncs.com/compatible-api/v1/reranks"
api_key_env = "DASHSCOPE_API_KEY"
```

服务地址以百炼当前控制台和文档为准，不假设不同工作空间或旧域名都能互换。

### 硅基流动

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

该固定维度 Embedding 示例不会发送 `dimensions` 参数；可变维度 Qwen3 模型需使用服务支持的模型名和维度。

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

`cache_dir` 内需要平铺 `model.onnx`、`tokenizer.json`、`config.json`、`special_tokens_map.json`、`tokenizer_config.json`。未提供离线文件时，只有设置 `allow_model_download = true` 才允许下载；需自行提供兼容的 ONNX Runtime。真实模型加载与推理尚未验证，编译和模拟测试不证明质量收益。

## 探测与诊断

`init --probe`、`index --probe`、`watch --probe`、`mcp --probe` 和 `doctor --models` 只使用固定测试句子，不读取源码作为探测输入，但仍会调用已配置模型并可能产生费用。默认启动不隐式探测、下载模型或触发重建。

`init --probe --config FILE` 检查传入配置的模型，生成的用户配置仍使用模型禁用的默认值。`mcp --probe` 的探测信息只写 stderr。

`doctor` 报告配置、数据库和模型检查；缺失/待升级索引为 warning，检测到数据库错误返回退出码 1，模型错误返回 4。普通只读查询遇到旧 schema 或分块规则返回 3。`doctor --prune-cache` 需要写入者锁，只删除未引用的缓存条目。

## 供应商文档

- [百炼同步 Embedding](https://docs.bailian.console.aliyun.com/zh/model-studio/text-embedding-synchronous-api)
- [百炼文本 Rerank](https://docs.bailian.console.aliyun.com/zh/model-studio/text-rerank-api)
- [硅基流动 Embeddings](https://api-docs.siliconflow.cn/docs/api/embeddings-post)
- [硅基流动 Rerank](https://api-docs.siliconflow.cn/docs/api/rerank-post)

当前验证边界见[实施与验证状态](implementation-status.md)。
