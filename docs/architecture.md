# 架构

## 模块与边界

| 包 | 职责 |
| --- | --- |
| `apps/solascope` | `solascope` 与 `sas` 两个入口、参数解析、输出格式、日志、rmcp stdio 服务 |
| `crates/solascope-core` | 共享数据结构、错误与退出码、`EmbeddingProvider` / `RerankProvider` / `MetadataStore` / `LexicalIndex` / `VectorIndex` 接口 |
| `crates/solascope-config` | TOML 分层配置、环境变量覆盖、校验与隐私约束 |
| `crates/solascope-scanner` | 文件发现、忽略与敏感路径过滤、受根目录约束的读取、文件监听 |
| `crates/solascope-parser` | Tree-sitter 定义提取、Chunk 生成与稳定标识 |
| `crates/solascope-models` | Ollama、通用/OpenAI 兼容、DashScope、硅基流动 HTTP 协议，以及可选 FastEmbed Rerank |
| `crates/solascope-storage` | SQLite 迁移、事务提交、向量缓存、FTS5、精确余弦检索、写入者锁 |
| `crates/solascope-engine` | 索引、查询、诊断与监听流水线编排 |
| `benchmarks` | 独立 `solascope-bench` 工具，直接调用 Engine 进行质量与性能评测 |

CLI、MCP 和基准工具调用同一套 `solascope_engine::Engine`。分块在 parser 中，词法/向量检索在 storage 中，索引和检索编排在 engine 中；公共接口定义在 core。

`Engine` 的克隆共享惰性初始化的模型适配器和查询向量缓存。本地 ONNX 模型不会在每次请求时重建。默认模型禁用；构建/启动核心程序不需要推理服务。

## 索引流水线

1. 获取索引目录的独占文件锁；已有写入者时立即以 `WriterBusy`（退出码 5）失败。首次创建索引目录内的 `.gitignore`，不覆盖已有文件。
2. 写入者按顺序迁移旧数据库；读操作不会迁移。比较当前 Embedding 指纹与已发布代际：默认 `strict` 拒绝不兼容增量写入；`fingerprint_policy = "rebuild"` 只在显式索引时转为重建。
3. 计算分块签名，包含 `CHUNKER_VERSION`、启用语言、回退策略、Chunk 预算和重叠行数。规则变化会重新解析所有文件，不把新旧分块混在同一可见索引中。
4. 扫描项目、计算 SHA-256，与已记录文件哈希比较，得到新增、修改、删除集合；使用有界 Rayon 线程池解析变化文件。
5. 按 `指纹 + 完整输入文本哈希` 查询向量缓存。非重建任务还可按路径无关的复用键找移动文件的向量，只为剩余文本请求 Embedding。
6. 模型请求在数据库写事务之外执行，按有界输入组、配置批次和并发执行。向量数量、维度和数值均校验；失败不发布半成品。
7. 提交前重新读取变化文件并核对哈希，同时复核待删除路径。期间文件又发生变化则放弃提交；监听模式会重新扫描并重试。
8. 在一个 SQLite 事务内更新文件、Chunk、FTS、向量映射、分块签名及代际记录，然后发布新的索引代际。

任何一步失败都不覆盖上一个已发布快照。中断任务通过 `index_jobs` 记录，下次索引标记恢复并重新扫描。显式重建即使面对空项目也生成新代际。日志报告扫描、Embedding 进度和提交，索引报告分别统计精确缓存命中、新向量和路径复用向量。

## 分块与位置映射

Rust、Python、JavaScript/JSX、TypeScript/TSX、Go 使用 Tree-sitter 定义提取。未知/禁用语言或损坏语法在允许回退时按行窗口分块，标记 `fallback`；不是跨文件类型检查或引用解析。

`fallback_policy = "skip"` 时，Engine 将无法生成 AST 的文件保存为无 Chunk 的文件记录，移除其旧定义，保留哈希并继续其他文件的索引；当轮跳过数进入报告和警告。底层 parser 的严格 API 仍返回解析错误，线程池或其他非解析错误不会被吞掉。

- 定义包含直接相邻的注释、属性、装饰器和声明包装；空行分隔的独立注释不强行附到后续定义。
- 类、impl、接口等容器由头部、独立定义和未单独提取的成员片段组成，保留字段、常量及其注释。
- 长定义优先选择预算内较浅的语法节点边界，这类分片不强制重叠；无合适边界时使用可重叠的行窗口，单行过长时在 UTF-8 边界拆分。
- `max_chunk_tokens × 4` 是字节预算估算，不是模型 tokenizer 的 token 保证。
- Chunk 保存原始源码的精确字节切片、1-based 闭区间行号、符号、签名、父级信息及内容哈希。覆盖测试检查字符边界、范围一致性和源码内容不遗漏。

## 标识、指纹与缓存

| 标识/缓存键 | 用途 |
| --- | --- |
| 实体 ID：项目、路径、限定符号名、类型的哈希 | 最终结果按实体去重 |
| Chunk ID：实体 ID 加分片序号 | 合并多路召回、关联 FTS 与向量 |
| Embedding 输入哈希 | 精确内容缓存；输入模板含路径、语言、符号、签名和正文，不含行号 |
| Embedding 指纹 | provider、模型、规范化 endpoint、维度、归一化及模板/维度请求策略；防止向量空间混用 |
| 路径无关复用键 | 模板版本、语言、符号、签名和正文哈希；识别只改变文件路径的代码 |
| 查询缓存键 | 指纹加裁去两端空白的查询哈希；进程内 TTL 5 分钟，容量上限 128 |

函数在同一文件内移动而完整输入不变时精确复用。文件移动/重命名会改变路径和完整输入，但可复用相同代码的旧路径向量。数据库将后者标为 `exact = false`，状态计入 `relocated_vectors`，属于近似复用。

`index --rebuild` 只复用精确缓存，重新计算路径近似复用向量；精确结果可以替换近似结果，反向覆盖被禁止。Reranker 更换不影响向量索引。密钥、并发、超时等不进入模型指纹。

## 模型协议

Embedding 与 Reranker 的 provider 独立配置，统一接口不混淆供应商格式：

- `EmbeddingProvider::embed` 对文档批次返回有序向量，`embed_query` 返回单条查询向量。DashScope 据此分别发送 `text_type = "document"` 与 `"query"`。
- 通用/OpenAI 兼容和硅基流动 Embedding 使用 `/v1/embeddings`、平铺 `input` 及索引化 `data`；DashScope 原生使用 `/api/v1/services/embeddings/text-embedding/text-embedding` 和嵌套 `input/parameters`。
- 通用、硅基流动和供应商所称的兼容 Rerank 使用平铺请求和顶层 `results`；DashScope 原生使用 `/api/v1/services/rerank/text-rerank/text-rerank` 和嵌套 `output.results`。兼容百炼可使用 `/compatible-api/v1/reranks`，不表示 OpenAI 公司定义了通用重排标准。
- `RerankProvider::limits` 声明候选数和字符上限。返回分数必须映射回原始文档索引和 ID，且有限、无重复、无越界。
- 本地 Rerank 在推理前检查真实 tokenizer 的完整查询/文档对长度；HTTP 服务仍受供应商自己的 token 上限约束，字符估算不能代替它。

HTTP 请求有超时、有限重试、响应大小上限，不跟随重定向。本地地址不走代理，云端传输需要显式隐私 opt-in。真实模型服务及 ONNX 推理的验证边界另见[实施状态](implementation-status.md)。

## 查询流水线

1. 校验查询、返回条数、路径过滤和索引分块规则。需要向量通道时确认指纹一致，再计算或复用查询向量；不兼容模型永不混查。
2. 在一个 SQLite 读事务快照内按模式召回：lexical 仅 FTS5 BM25，semantic 仅精确向量，hybrid 使用词法、符号（精确 > 前缀 > 包含）和可选向量。hybrid 的数据库通道依次执行，共用同一快照。
3. 按 Chunk ID 合并来源，使用 RRF 融合，不直接相加各检索器原始分数。看起来是符号的查询优先精确定义；无名文本、测试/fixture/example 路径使用小幅权重调整，显式查询或过滤这些辅助代码时不惩罚路径。
4. 重新读取候选文件：修改、删除或读到变化中的文件标记 `stale`；被当前隐私/忽略规则屏蔽的候选剔除。源码读取仍受项目根约束。
5. 可选地将非 stale 的领先候选交给 Reranker，按配置及 provider 限额截断候选文本。校验评分映射后重排，精确符号优先仍保留；重排之后再次核对可见性与源文件状态。
6. 按实体去重，返回前 `limit` 条，以及警告和阶段耗时。

`find_symbol` 额外支持有限编辑距离的拼写纠错；字面匹配优先，模糊结果标为 `fuzzy`。混合搜索不使用这些模糊符号候选。

Embedding 临时不可用或认证失败时，允许的 hybrid 查询降级为词法加符号；纯 semantic 查询和模型不兼容错误不降级。Reranker 失败时按 `fail_open` 保留融合排序或报错。

`SearchResponse.timings` 包含 Embedding、重排、其余本地检索和总墙钟毫秒数。本地检索时间包括融合与源码校验，不只是 SQL。缓存命中时 `embedding_ms = 0`，不能据暖缓存结果推断真实模型延迟。

## SQLite 与迁移

SQLite 使用 WAL、外键和单写入者锁。表包括 `projects`、`files`、`chunks`、`chunk_fts`（FTS5）、`embedding_cache`、`chunk_embeddings`、`index_generations`、`index_jobs`，以及保存分块签名的 `meta`。FTS 行通过 `chunks` 触发器维护，与普通表在同一事务提交。

向量以小端 `f32` BLOB 持久化，查询时逐行计算精确余弦相似度。首版优先逻辑正确，不能保证百万级 Chunk 的毫秒级检索；可通过 `VectorIndex` 替换为 ANN 后端后再做相同评测集对照。

`PRAGMA user_version` 表示已应用的迁移数，当前为 2：

- `migrations/0001_initial.sql`：初始 schema。
- `migrations/0002_metadata_and_relocation.sql`：分块元数据、路径复用键与精确标记；迁移后从已保存的 Chunk 回填复用键。

每个迁移的 SQL、回填和版本更新在同一事务内执行；已发布迁移不改写，只追加。写入者可原地升级旧索引，旧 schema 的只读命令返回 `IndexOutdated`（退出码 3）并提示先运行 `index`。新于当前程序的 schema 拒绝打开，提示升级程序或换索引目录，**不要求删除原索引**。数据库在迁移或切换 journal mode 前先核对项目归属，拒绝被其他项目根复用；错误根目录不能改变旧 schema。无版本的无关数据库也不会被当作 SolaScope 索引迁移。

代际保留配置保留的是版本记录，不是可任意回滚的完整历史源码副本。`doctor` 检查数据库与 FTS 一致性；`--prune-cache` 只清理未引用向量，需写入者锁。

## CLI、MCP、监听与日志

- `sas` 是独立编译的第二个入口，复用同一 CLI 实现，不依赖 shell 别名。
- 索引默认在项目内 `.solascope/`，也可以配置其他目录；索引目录不能包含项目根，源文件读取始终限定在项目根。
- MCP 是四工具的只读 stdio 服务，不自动索引或迁移。`watch` 或 `index --watch` 在另一个进程更新，WAL 允许读到完整的已发布代际。
- 监听先开启文件事件流，再初始扫描；事件防抖后执行哈希驱动的增量扫描，提交前修订复核，源变化则重试。它不是每文件持久化任务队列；丢失事件可用显式 `index` 修复。
- `index.watch` 可由 `--no-watch` 关闭；持续监听的 JSON 模式只输出首次索引报告，后续更新写日志。
- 默认日志写 stderr。文件日志只追加拥有 SolaScope 标识的项目内文件，配置的日志路径从源码索引/读取中排除；新建 Unix 日志权限为 0600。
- 配置密钥值始终脱敏，默认也隐藏一般 Bearer token；详细级别默认只开启 SolaScope 模块，避免隐式打印协议正文。显式 `RUST_LOG` 需谨慎使用。
- `--probe` 和 `doctor --models` 用固定句子探测配置模型，不发送源文件；MCP 探测信息只写 stderr。未显式请求时不进行启动推理探测。

## 当前取舍与验证

当前有可运行的默认无模型 CLI/MCP、模拟协议测试、质量/性能工具和三平台 CI/ZIP 打包配置。模拟响应验证请求契约、映射、错误与降级，不能证明真实语义检索或重排收益。

真实云端/Ollama 服务、本地 ONNX 模型、macOS/Windows 和远程 GitHub runner 尚未实际验证；未发布远程 release。完整里程碑边界见[实施与验证状态](implementation-status.md)，评测方法见[benchmarks/README.md](../benchmarks/README.md)。
