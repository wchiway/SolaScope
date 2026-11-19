# 支持范围

当前版本为 v0.1.0。

## 功能

| 部分 | 支持内容 |
| --- | --- |
| CLI | `solascope` 和 `sas` 共用实现；索引、搜索、符号、上下文、诊断和监听 |
| 解析 | Rust、Python、JavaScript/JSX、TypeScript/TSX、Go 定义提取；其他文本按行分块 |
| 存储 | SQLite/FTS5、增量更新、schema 迁移、向量缓存、单写入者和读事务快照 |
| 检索 | 词法、符号、精确向量检索及 RRF 融合；结果带行号、来源和 stale 状态 |
| Embedding | Ollama、通用/OpenAI 兼容、百炼 DashScope、硅基流动 |
| 重排 | 通用兼容 HTTP、百炼、硅基流动，以及可选的 FastEmbed/ONNX |
| MCP | stdio 传输的四个只读工具，与 CLI 共用 Engine |
| 评测 | 标注查询质量指标、索引耗时、查询百分位和 Linux 驻留内存采样 |

配置见[配置文档](configuration.md)，实现见[架构](architecture.md)，评测记录见[基准数据](benchmarks.md)。

## 测试

Linux x86_64 上运行了工作区测试、Clippy、格式检查和 CLI 端到端验证。HTTP 模型协议使用本地模拟服务；MCP 测试使用原生 JSON-RPC 和 rmcp 客户端。

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --all-targets --locked --features local-rerank
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

尚未实测真实模型服务、本地 ONNX 推理、macOS/Windows 和商业 MCP 客户端。三平台 CI 与打包工作流已配置，远程 runner 和发行包仍需验证。

## 现有限制

- 向量检索是精确扫描，没有 ANN 后端。
- 解析只提取语法定义，不提供跨文件引用或调用图。
- 首版本地 Embedding 依赖 Ollama；不包含进程内 Embedding。
- 无多项目聚合、共享缓存、守护进程、量化或索引压缩。
- 不持久化重排结果；hybrid 的数据库召回通道依次执行。
