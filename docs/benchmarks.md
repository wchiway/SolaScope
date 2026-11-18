# 基准与质量数据

以下数字来自一次本地实测，用于说明量级，不是性能承诺。换用其他硬件、语料或启用模型后需要重新测量。

## 环境

- 构建：SolaScope v0.1.0，`cargo build --release`
- CPU：4 个逻辑核
- 系统：Linux x86_64，rustc 1.99.0
- 模型：全部禁用（仅词法与符号检索）

## 语料

tokio 1.54.1、hyper 1.12.0、reqwest 0.12.28、rusqlite 0.43.0、clap_builder 4.6.7、rmcp 3.11.0 的发布源码，加上本项目的 `apps/` 与 `crates/`。

- 磁盘占用约 15 MiB
- 索引 1069 个文件，生成 30 273 个 Chunk
- 语言几乎全部是 Rust

## 索引

| 操作 | 耗时 | 峰值内存 |
| --- | --- | --- |
| 首次索引 | 3.6 s | 109 MB |
| 无变化的再次索引 | 0.25 s | 29 MB |
| 修改 1 个文件后的增量索引 | 0.26 s | 28 MB |
| `--rebuild` 全量重建 | 4.6 s | 108 MB |

索引数据库 103 MB，约为源码体积的 7 倍：每个 Chunk 保存了正文、Embedding 输入文本和一份 FTS 副本。v0.1 没有做压缩或去重。

无变化时的耗时主要是重新读取并哈希全部文件。

## 查询

每项连续运行 30 次，计时包含进程启动、打开数据库和输出 JSON 的全部时间。

| 查询 | P50 | P95 |
| --- | --- | --- |
| 词法：`spawn_blocking` | 21.4 ms | 22.2 ms |
| 词法：`connection closed before message completed` | 24.7 ms | 26.1 ms |
| 词法：`how is the runtime shut down gracefully` | 29.6 ms | 30.6 ms |
| 混合（无模型）：同上 | 29.8 ms | 30.7 ms |
| 词法加路径过滤：`shutdown`，`--path tokio-1.54.1/src` | 24.2 ms | 25.3 ms |
| 符号：`spawn_blocking` | 16.5 ms | 17.2 ms |
| 符号：无匹配 | 13.3 ms | 14.3 ms |

符号查询目前对符号名做子串匹配，需要扫描全部符号索引项，耗时随 Chunk 数线性增长。

向量检索、Embedding 请求和 Rerank 的延迟没有测量：测试环境没有可用的模型服务。

## 检索质量

### 标注样例集

`tests/fixtures/multilang/` 含 5 个文件（Rust、Python、TypeScript、JavaScript、Go 各一），`queries.json` 标注了 18 条查询的期望文件和符号，其中 5 条标注了行区间。运行：

```bash
cargo test -p solascope-engine --test quality -- --nocapture
```

| 模式 | Top1 | Recall@5 | MRR@10 |
| --- | --- | --- | --- |
| lexical | 1.000 | 1.000 | 1.000 |
| hybrid（无模型） | 1.000 | 1.000 | 1.000 |

这组数据只能说明流水线在五种语言上能返回正确的文件、符号和行号，**不能代表真实仓库上的检索质量**：样例只有 28 个 Chunk，且自然语言查询的用词与代码注释高度重合。

### 真实语料上的观察

在上面的 Rust 语料上，标识符和报错字符串查询能直接命中定义。自然语言查询 `how is the runtime shut down gracefully` 在无模型时返回的前三条是测试文件中的无符号文本块，没有命中运行时关闭的实现。无模型时的排序完全依赖词项重合，这类查询需要启用 Embedding。

### 尚未完成

- vector-only、hybrid 含向量、hybrid 加 rerank 的对照：需要真实模型服务。
- 真实项目上的标注问题集。
- 不同规模仓库的对比，以及 `watch`、CLI、MCP 并发时的延迟。
