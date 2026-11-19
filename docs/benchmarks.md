# 基准数据

以下记录来自 Linux x86_64、4 个逻辑核、Rust 1.99，Embedding 和 Reranker 均禁用。数据用于观察索引和查询的量级，不是延迟保证。不同构建模式、分块版本和计时范围不能直接比较。

评测工具用法见 [benchmarks/README.md](../benchmarks/README.md)。

## 调试构建记录

测量使用 `solascope-bench`，源文件复制到独立目录，索引放在新的临时目录中。索引各阶段执行一次；查询先执行一次，再重复 30 次。

| 语料 | 文件 | Chunk | 首次索引 | 无变化扫描 | 重建 | 重建后 VmHWM |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 五语言 fixture | 5 | 22 | 46.4 ms | 9.4 ms | 16.3 ms | 21.4 MiB |
| tokio 1.54.1 | 582 | 10 560 | 4.21 s | 0.365 s | 5.17 s | 58.7 MiB |
| 六个 Rust 库及项目源码 | 1073 | 26 966 | 14.23 s | 0.757 s | 12.50 s | 108.4 MiB |

第三组包含 tokio 1.54.1、hyper 1.12.0、reqwest 0.12.28、rusqlite 0.43.0、clap_builder 4.6.7、rmcp 3.11.0 和项目的 `apps/`、`crates/`。首次索引目录约 93.7 MiB，重建后约 102.8 MiB。

VmHWM 是进程的累计驻留内存高水位，不是某个阶段独占的内存。索引时间只有一次观测；重建比首次索引快不代表稳定收益。

### 查询

下表来自较大语料，P50/P95 使用每条查询后续的 30 次观测。计时包含 `Engine::search` 内的召回、融合和源码校验，不包含进程启动及 JSON 输出。

| 查询 | 模式 | P50 | P95 |
| --- | --- | ---: | ---: |
| `spawn_blocking` | lexical | 20.9 ms | 22.7 ms |
| `connection closed before message completed` | lexical | 75.0 ms | 76.7 ms |
| `how is the runtime shut down gracefully` | lexical | 121.3 ms | 130.3 ms |
| `spawn_blocking` | hybrid，无模型 | 43.8 ms | 45.3 ms |
| `connection closed before message completed` | hybrid，无模型 | 95.7 ms | 101.5 ms |
| `how is the runtime shut down gracefully` | hybrid，无模型 | 141.0 ms | 149.0 ms |

lexical 使用 FTS5；无模型 hybrid 还包含独立符号通道。semantic 和 hybrid+rerank 未测量。小语料使用 fixture 的 18 条查询，其余两组使用以上三个查询。

查询文件可写为：

```json
[
  "spawn_blocking",
  "connection closed before message completed",
  "how is the runtime shut down gracefully"
]
```

```bash
cargo run -p solascope-bench -- perf --root /path/to/copied-corpus \
  --queries /path/to/queries.json --config /path/to/models-disabled.toml \
  --runs 30 --json
```

以上命令使用调试构建，与表中数据一致。做正式性能对比时加 `--release`，并固定语料、查询集、模型配置和缓存条件。

## 早期发布构建记录

早期 v0.1.0 使用相同六个 Rust 库及项目源码，索引了 1069 个文件、30 273 个 Chunk。其分块规则与上述记录不同，查询计时还包含进程启动、数据库打开和 JSON 输出。

| 操作 | 耗时 | 峰值内存 |
| --- | ---: | ---: |
| 首次索引 | 3.6 s | 109 MB |
| 无变化扫描 | 0.25 s | 29 MB |
| 修改一个文件后索引 | 0.26 s | 28 MB |
| 全量重建 | 4.6 s | 108 MB |

| 查询 | P50 | P95 |
| --- | ---: | ---: |
| lexical：`spawn_blocking` | 21.4 ms | 22.2 ms |
| lexical：`connection closed before message completed` | 24.7 ms | 26.1 ms |
| lexical：`how is the runtime shut down gracefully` | 29.6 ms | 30.6 ms |
| hybrid，无模型：同上 | 29.8 ms | 30.7 ms |
| lexical：`shutdown`，路径限于 tokio 源码 | 24.2 ms | 25.3 ms |
| symbol：`spawn_blocking` | 16.5 ms | 17.2 ms |
| symbol：无匹配 | 13.3 ms | 14.3 ms |

数据库约 103 MB，源码约 15 MiB。索引存储正文、Embedding 输入文本和 FTS 数据；未做压缩。向量与重排延迟未测量。

## Fixture 质量

`tests/fixtures/multilang/` 包含五种语言的源文件及 18 条标注查询，其中 5 条标注了行区间。

```bash
cargo test -p solascope-engine --test quality -- --nocapture
```

| 模式 | Top1 | Recall@5 | MRR@10 | 错误定位率 |
| --- | ---: | ---: | ---: | ---: |
| lexical | 1.000 | 1.000 | 1.000 | 0 |
| hybrid，无模型 | 1.000 | 1.000 | 1.000 | 0 |

这组查询验证文件、符号与行号匹配。样本小，查询词与注释重合较多，不能据此判断真实项目的语义检索质量。真实模型对照、更多项目的标注集，以及 watch/CLI/MCP 并发延迟仍待测量。
