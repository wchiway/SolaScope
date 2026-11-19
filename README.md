# SolaScope

本地优先的代码库索引与定位工具。把自然语言问题、符号名、关键字或报错信息映射到**文件、符号和代码行**，同时提供 CLI（`solascope` / `sas`）和 stdio MCP 服务。

- 默认不需要任何模型，也不上传代码：词法（SQLite FTS5）和符号检索开箱可用。
- Embedding 与 Reranker 可分别配置为本地或远程，启用后与词法、符号结果做 RRF 融合。
- 增量索引：只重新解析变化的文件，只为新内容请求 Embedding。
- 结果给出项目相对路径、符号、1-based 起止行和片段；文件在索引后被修改时标记 `stale`。

当前版本 v0.1.0。已验证与未验证的范围见[验证状态](#验证状态)。

## 安装

需要 Rust 工具链；开发和验证使用 1.99，更低版本未测试。SQLite 随程序静态编译，无需额外服务。

```bash
cargo build --release            # 产物：target/release/solascope 与 target/release/sas
cargo install --path apps/solascope --locked   # 或安装到 ~/.cargo/bin
```

`solascope` 与 `sas` 是同一程序的两个入口，子命令、参数、输出完全一致。

## 快速开始

```bash
cd your-project
solascope init                    # 创建用户目录 ~/.solascope/.solascope.toml，不覆盖已有文件
solascope index                   # 首次建立索引，之后再次运行即增量更新
sas search "reserve stock for an order"
sas symbol readSession
sas context inventory.go:14 --before 1 --after 3
sas status
```

搜索输出形如 `path:起始行:结束行  符号  分数`，后跟代码片段：

```text
inventory.go:13:20  Inventory.Reserve  0.0164
func (inventory *Inventory) Reserve(sku string, quantity int) error {
```

用户配置位置：Linux/macOS 为 `~/.solascope/.solascope.toml`，Windows 为 `$USERPROFILE/.solascope/.solascope.toml`。同目录的 `.env` 可保存 `DASHSCOPE_API_KEY`、`SILICONFLOW_API_KEY` 等凭据，无须单独 export；已有进程环境变量优先。`.env` 不被索引、读取为代码上下文或输出到日志。

`init` 不写项目配置，也不绑定当前项目。项目配置仍可手动创建或用 `--config` 指定。没有任何配置时也可以直接 `index`，使用默认值；项目根由命令位置参数、`--root`、`SOLASCOPE__PROJECT__ROOT` 和当前目录的项目标记确定。

## 命令

| 命令 | 作用 |
| --- | --- |
| `init [--probe]` | 在用户目录 `.solascope/` 创建 `.solascope.toml`，不覆盖；`--probe` 检查传入配置的模型 |
| `index [path] [--rebuild] [--probe]` | 建立或增量更新索引；`--rebuild` 全量重建并生成新的索引代际 |
| `index [path] --watch` | 首次扫描后持续监听；`--no-watch` 可覆盖 `index.watch = true` |
| `search <query>` | 混合搜索，JSON 结果另含各阶段耗时 |
| `symbol <name> [--kind K]` | 精确、前缀、包含和有限拼写纠错；模糊结果标记 `fuzzy` |
| `context <path>[:line]` | 读取源码上下文，支持 `--line/--before/--after` 或 `--start-line/--end-line`，最多 200 行 / 64 KiB |
| `status` | 索引代际、文件数、Chunk 数、向量数、路径复用向量数及模型指纹 |
| `watch [path] [--probe]` | 持续监听文件变化并增量索引，Ctrl-C 退出 |
| `mcp [--probe]` | 启动 stdio MCP 服务（只读）；探测信息只写 stderr |
| `doctor [--models] [--prune-cache]` | 检查配置和数据库一致性；`--models` 探测模型；`--prune-cache` 清理未引用缓存；检测到错误时返回非零退出码 |

通用参数：`--config`、`--root`、`--json`、`--limit`（1–100）、`--mode lexical|semantic|hybrid`、`--no-rerank`、`--path`（相对路径前缀或 glob）、`--verbose`。

`lexical` 仅用 FTS5 召回，`semantic` 仅用向量召回；`hybrid` 融合词法、符号和可选向量。各模式均可独立关闭重排以做对照。

`--json` 时 stdout 只输出一个 JSON 结果；警告和错误写到 stderr，进度日志遵循 `log.output`（默认 stderr）。`index --watch --json` 先输出首次索引结果，后续更新只写日志。

退出码：

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 内部错误、存储或 I/O 错误 |
| 2 | 参数、配置或隐私边界错误 |
| 3 | 索引不存在，或 schema/分块规则需要升级 |
| 4 | 模型不可用、认证失败，或模型与索引不兼容 |
| 5 | 已有另一个写入者（`index`/`watch`）持有索引 |
| 130 | 被中断 |

## 模型

默认 `embedding.provider = "disabled"`、`rerank.provider = "disabled"`。此时 `hybrid` 模式使用词法加符号召回，`semantic` 模式会以退出码 4 报错。

| 能力 | provider | 说明 |
| --- | --- | --- |
| Embedding | `ollama` | 本机 Ollama 的 `/api/embed`，默认 `nomic-embed-text` / 768 维；需自行安装 Ollama 并 `ollama pull` 模型 |
| Embedding | `openai`、`remote` | 通用/OpenAI 兼容的 `/v1/embeddings`，支持省略维度请求参数 |
| Embedding | `dashscope` | 百炼原生同步接口，文档和查询分别使用对应 `text_type` |
| Embedding | `siliconflow` | 硅基流动 `/v1/embeddings`，区分固定维度与可变维度模型 |
| Rerank | `remote`、`openai` | 通用平铺请求，支持完整 `/rerank` 或 `/reranks` URL |
| Rerank | `dashscope` | 百炼原生文本重排，读取嵌套 `output.results` |
| Rerank | `siliconflow` | 硅基流动 `/v1/rerank`，保留原始候选索引和 ID |
| Rerank | `fastembed` | 进程内 ONNX 重排，需用 `--features local-rerank` 编译并自备 ONNX Runtime |

云端 provider 均要求 `privacy.remote_opt_in = true`。Rerank 的 `openai` 表示供应商的兼容请求格式，不代表 OpenAI 公司提供统一的重排 API；百炼兼容接口可使用 `/compatible-api/v1/reranks`。配置示例和官方文档链接见 [docs/configuration.md](docs/configuration.md)。

每个向量索引绑定一个 Embedding 指纹（provider、模型、endpoint、维度、归一化、输入模板版本）。默认 `fingerprint_policy = "strict"`：修改其中任何一项后，`index` 和带向量的搜索会以退出码 4 拒绝运行，需要执行 `solascope index --rebuild`。设为 `"rebuild"` 时，下一次显式 `index` 自动重建；查询不会触发写入。更换 Reranker 不需要重建。

修改解析器设置或升级分块规则后，下次 `index` 会自动重新解析所有文件，复用仍匹配的缓存。旧 schema 或旧分块规则的只读查询返回退出码 3，提示先执行 `index`。

移动或重命名文件时，相同语言、符号、签名和正文可复用旧路径的向量；这类近似复用记录为 `relocated_vectors`，显式 `--rebuild` 会用当前完整输入重新计算。

Embedding 临时不可用时，`hybrid` 搜索降级为词法加符号并输出警告（`search.semantic_fail_open`）；Reranker 失败时保留融合排序（`rerank.fail_open`）。

配置字段、环境变量和示例见 [docs/configuration.md](docs/configuration.md)。

## MCP

```bash
solascope mcp --root /abs/path/to/project
```

提供四个只读工具：`search_code`、`find_symbol`、`get_code_context`、`index_status`。服务不会自动建立或更新索引，需要另外运行 `index` 或 `watch`。客户端配置和工具参数见 [docs/mcp.md](docs/mcp.md)。

## 隐私与安全边界

- 未显式设置 `privacy.remote_opt_in = true` 时，任何远程模型（包括非本机地址的 Ollama）都会在配置校验阶段被拒绝。
- 以下内容永不进入索引，也不能通过 `context` 读取，且不能被 include 规则解除：`.env`、`.env.*`、私钥和证书文件（`.pem`、`.key`、`id_rsa` 等）、`.ssh`、`.aws`、`secrets`、`credentials` 等目录，以及 `.git`、`target`、`node_modules` 等构建和依赖目录。
- 遵循 `.gitignore` 和 `.solascopeignore`；可用 `project.exclude`、`privacy.sensitive_path_patterns` 追加规则。
- 所有路径限定在项目根内：拒绝 `..`、绝对路径和指向根外的符号链接。
- API Key 只通过环境变量名引用，不写入配置、日志、错误信息或指纹；endpoint 不允许携带凭据或查询串。
- 索引默认位于项目内的 `.solascope/`，其中包含源码片段，请勿提交或分享。

## 语言支持

Rust、Python、JavaScript（含 JSX）、TypeScript（含 TSX）、Go 使用 Tree-sitter 提取函数、方法、类、结构体、接口、枚举等定义。其他文本文件，以及语法树含错误的文件，按行窗口分块并标记为 `fallback`，仍可被词法检索。

相邻的文档注释、属性和装饰器与其定义一同索引；容器内的字段、常量等未单独定义的内容仍保留在容器块中。长定义优先在语法节点边界拆分，无法使用语法边界时才回退到行窗口。

这是语法级的定义提取，不解析跨文件引用或调用关系。

## 可复现评测

```bash
cargo run --release -p solascope-bench -- quality \
  --root tests/fixtures/multilang --queries tests/fixtures/multilang/queries.json
cargo run --release -p solascope-bench -- perf \
  --root /path/to/repository --queries /path/to/queries.json --runs 30 --json
```

工具报告分模式/类别的 Top1、Recall@5、MRR@10、错误定位率，以及索引、重建、查询 P50/P95 和内存采样。模型未启用的模式会明确跳过。见 [benchmarks/README.md](benchmarks/README.md)。

## 验证状态

Linux x86_64 上运行了工作区测试、Clippy、格式检查和 CLI 端到端验证，包括 `local-rerank` 特性。CLI 发布构建和本地安装已验证。

模型协议测试使用本地模拟服务。真实模型质量、本地 ONNX 推理、macOS/Windows 和商业 MCP 客户端尚未实测。三平台 CI 与 ZIP 打包工作流已配置，远程 runner 和发行包仍需验证。

测试范围见[支持范围](docs/implementation-status.md)，性能测量见[基准数据](docs/benchmarks.md)。

## 开发

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test -p solascope-engine --test quality -- --nocapture   # 打印检索质量指标
```

代码结构和关键设计决策见 [docs/architecture.md](docs/architecture.md)。

## 许可证

本项目以 [Apache License 2.0](LICENSE) 发布。
