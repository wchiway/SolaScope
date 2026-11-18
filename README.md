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
solascope init                    # 生成 .solascope.toml（已存在则不覆盖）
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

不执行 `init` 也可以直接 `index`，此时使用默认配置。项目根按以下顺序确定：命令的位置参数路径 > `--root` > `SOLASCOPE__PROJECT__ROOT` > 从当前目录向上找到的 `.solascope.toml` 或 `.git`。

## 命令

| 命令 | 作用 |
| --- | --- |
| `init [path]` | 写入默认配置，不覆盖已有文件 |
| `index [path] [--rebuild]` | 建立或增量更新索引；`--rebuild` 全量重建并生成新的索引代际 |
| `search <query>` | 混合搜索 |
| `symbol <name> [--kind K]` | 查找符号定义 |
| `context <path>[:line]` | 读取源码上下文，支持 `--line/--before/--after` 或 `--start-line/--end-line`，最多 200 行 / 64 KiB |
| `status` | 索引代际、文件数、Chunk 数、向量数、模型指纹 |
| `watch [path]` | 持续监听文件变化并增量索引，Ctrl-C 退出 |
| `mcp` | 启动 stdio MCP 服务（只读） |
| `doctor [--models] [--prune-cache]` | 检查配置和数据库一致性；`--models` 实际探测模型服务；`--prune-cache` 清理不再被引用的向量缓存 |

通用参数：`--config`、`--root`、`--json`、`--limit`（1–100）、`--mode lexical|semantic|hybrid`、`--no-rerank`、`--path`（相对路径前缀或 glob）、`--verbose`。

`--json` 时 stdout 只输出一个 JSON 结果；警告、进度和错误始终写到 stderr。

退出码：

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 内部错误、存储或 I/O 错误 |
| 2 | 参数、配置或隐私边界错误 |
| 3 | 索引不存在 |
| 4 | 模型不可用、认证失败，或模型与索引不兼容 |
| 5 | 已有另一个写入者（`index`/`watch`）持有索引 |
| 130 | 被中断 |

## 模型

默认 `embedding.provider = "disabled"`、`rerank.provider = "disabled"`。此时 `hybrid` 模式使用词法加符号召回，`semantic` 模式会以退出码 4 报错。

| 能力 | provider | 说明 |
| --- | --- | --- |
| Embedding | `ollama` | 本机 Ollama 的 `/api/embed`，默认 `nomic-embed-text` / 768 维；需自行安装 Ollama 并 `ollama pull` 模型 |
| Embedding | `openai` | OpenAI 兼容的 `/v1/embeddings`；属于远程传输，需要 `privacy.remote_opt_in = true` |
| Rerank | `remote` | `POST {endpoint}/rerank`（Cohere/Jina 风格）；需要 `privacy.remote_opt_in = true` |
| Rerank | `fastembed` | 进程内 ONNX 重排，需用 `--features local-rerank` 编译并自备 ONNX Runtime |

每个向量索引绑定一个 Embedding 指纹（provider、模型、endpoint、维度、归一化、输入模板版本）。修改其中任何一项后，`index` 和带向量的搜索会以退出码 4 拒绝运行，需要执行 `solascope index --rebuild`。更换 Reranker 不需要重建。

Embedding 临时不可用时，`hybrid` 搜索降级为词法加符号并输出警告（`search.semantic_fail_open`）；Reranker 失败时保留融合排序（`rerank.fail_open`）。

配置字段、环境变量和示例见 [docs/configuration.md](docs/configuration.md)。

## MCP

```bash
solascope mcp --root /abs/path/to/project
```

提供四个只读工具：`search_code`、`find_symbol`、`get_code_context`、`index_status`。服务不会自动建立或更新索引，需要另外运行 `index` 或 `watch`。客户端配置和工具参数见 [docs/mcp.md](docs/mcp.md)。

## 隐私与安全边界

- 未显式设置 `privacy.remote_opt_in = true` 时，任何远程模型（包括非本机地址的 Ollama）都会在配置校验阶段被拒绝。
- 以下内容永不进入索引，也不能通过 `context` 读取，且不能被 include 规则解除：`.env*`、私钥和证书文件（`.pem`、`.key`、`id_rsa` 等）、`.ssh`、`.aws`、`secrets`、`credentials` 等目录，以及 `.git`、`target`、`node_modules` 等构建和依赖目录。
- 遵循 `.gitignore` 和 `.solascopeignore`；可用 `project.exclude`、`privacy.sensitive_path_patterns` 追加规则。
- 所有路径限定在项目根内：拒绝 `..`、绝对路径和指向根外的符号链接。
- API Key 只通过环境变量名引用，不写入配置、日志、错误信息或指纹；endpoint 不允许携带凭据或查询串。
- 索引默认位于项目内的 `.solascope/`，其中包含源码片段，请勿提交或分享。

## 语言支持

Rust、Python、JavaScript（含 JSX）、TypeScript（含 TSX）、Go 使用 Tree-sitter 提取函数、方法、类、结构体、接口、枚举等定义。其他文本文件，以及语法树含错误的文件，按行窗口分块并标记为 `fallback`，仍可被词法检索。

这是语法级的定义提取，不解析跨文件引用或调用关系。

## 验证状态

在 Linux x86_64上已验证：

- `cargo test --workspace --all-targets`：90 项测试通过，覆盖配置优先级、扫描与隐私过滤、五种语言解析、存储事务、增删改与重命名的增量索引、Embedding 缓存复用、模型指纹隔离、降级、监听防抖、单写入者锁、CLI 双入口，以及原生 JSON-RPC 和 rmcp 客户端两种方式的 MCP 端到端调用。
- `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --all -- --check` 通过。
- 发布版构建，以及在约 1000 个 Rust 文件上的索引和查询实测，见 [docs/benchmarks.md](docs/benchmarks.md)。

尚未验证：

- **真实模型服务**：Ollama、OpenAI 兼容接口和远程 Rerank 适配器只用模拟 HTTP 服务测试过协议与错误处理，没有连接过真实服务，也没有语义检索质量数据。
- **本地 ONNX 重排**：`local-rerank` 特性只做了编译检查，没有用真实模型运行过。
- **macOS 与 Windows**：未构建、未运行。
- **商业 MCP 客户端**：只用测试内的两种客户端验证过协议。

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
