# 架构

## 模块

| 包 | 职责 |
| --- | --- |
| `apps/solascope` | `solascope` 与 `sas` 两个入口、参数解析、输出格式、rmcp stdio 服务 |
| `crates/solascope-core` | 共享数据结构、错误类型与退出码、`EmbeddingProvider` / `RerankProvider` / `MetadataStore` / `LexicalIndex` / `VectorIndex` 接口 |
| `crates/solascope-config` | TOML 分层配置、环境变量覆盖、校验与隐私约束 |
| `crates/solascope-scanner` | 文件发现、忽略与敏感路径过滤、受根目录约束的读取、文件监听 |
| `crates/solascope-parser` | Tree-sitter 定义提取、Chunk 生成与稳定标识 |
| `crates/solascope-models` | Ollama、OpenAI 兼容 Embedding，HTTP 与 FastEmbed Rerank |
| `crates/solascope-storage` | SQLite schema、事务提交、FTS5、精确余弦检索、写入者锁 |
| `crates/solascope-engine` | 索引流水线、查询流水线、诊断与监听的编排 |

CLI 和 MCP 都只调用 `solascope_engine::Engine`，不直接访问数据库或模型。

分块在 parser 中，词法与向量检索在 storage 中，索引与检索编排在 engine 中；公共接口定义在 core。

## 索引流水线

1. 获取索引目录的独占文件锁；已有写入者时立即以 `WriterBusy` 失败。
2. 比较当前 Embedding 指纹与索引记录的指纹，不一致且未指定 `--rebuild` 时拒绝运行。
3. 扫描项目，计算文件 SHA-256；与已记录的哈希比较，得到新增、修改、删除的文件集合。
4. 并行解析变化的文件，生成 Chunk。
5. 按 `指纹 + 输入文本哈希` 查询向量缓存，只为未命中的文本请求 Embedding。模型请求发生在数据库写事务之外。
6. 提交前重新读取每个变化文件并核对哈希；期间文件又被修改则放弃本次提交（`watch` 会自动重试）。
7. 在单个 SQLite 事务内更新文件、Chunk、FTS、向量映射并发布新的索引代际。

任何一步失败，上一个已发布的快照保持不变。被中断的任务在下次索引时标记为已恢复。

## 标识与缓存键

- **实体 ID**：项目、路径、限定符号名、类型的哈希，用于结果去重。
- **Chunk ID**：实体 ID 加分片序号。
- **Embedding 输入**：模板版本、路径、语言、符号、签名和代码，不包含行号。函数在文件内移动不会改变输入，因而复用缓存；重命名或移动文件会改变路径，需要重新请求。
- **指纹**：provider、模型、规范化的 endpoint、维度、归一化方式、模板版本。

## 查询流水线

1. 需要语义通道时计算查询向量（进程内缓存 5 分钟），并确认指纹与索引一致。
2. 在一个读事务快照内执行词法（FTS5 BM25，符号列和路径列加权）、符号（精确 > 前缀 > 包含）和向量（精确余弦）召回。
3. 用 RRF 按排名融合，不直接相加各通道的原始分数。查询看起来是标识符时，精确命中的符号排在最前。
4. 重新读取候选所在文件：哈希变化或文件已删除的标记 `stale`，被当前忽略或隐私规则屏蔽的直接剔除。
5. 可选地把靠前的候选交给 Reranker，校验返回的候选映射后重排；重排结束后再次执行第 4 步。
6. 按实体去重，截取前 `limit` 条。

## 存储

SQLite（WAL 模式），表：`projects`、`files`、`chunks`、`chunk_fts`（FTS5）、`embedding_cache`、`chunk_embeddings`、`index_generations`、`index_jobs`。FTS 行由 `chunks` 上的触发器维护。只读命令以只读方式打开数据库，不会创建索引。

向量以小端 `f32` BLOB 存储，查询时逐行计算余弦相似度。这是首版为保证正确性所做的选择，规模增大后需要通过 `VectorIndex` 接口替换为近似检索。

schema 版本记录在 `user_version`，当前为 1。v0.1 没有迁移逻辑：版本不匹配时程序报错，需要删除索引目录后重新索引。

## 首版的取舍

- 索引目录固定在项目内，默认 `.solascope/`。
- `sas` 是独立编译的第二个可执行文件，不依赖 shell 别名。
- `mcp` 只读；持续更新由单独的 `watch` 进程负责，两者通过 WAL 并发读写。
- 多进程写入采用文件锁，失败即提示，不排队等待。
- 远程 Rerank 只实现了一种请求格式（`/rerank`，`results[].index` 与 `relevance_score`）。
