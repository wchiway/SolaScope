# 可复现的质量和性能评测

`solascope-bench` 直接调用共享 `Engine`，不会另外实现检索逻辑。它不是默认安装的 CLI，可从源码运行：

```bash
cargo run --release -p solascope-bench -- quality \
  --root tests/fixtures/multilang \
  --queries tests/fixtures/multilang/queries.json

cargo run --release -p solascope-bench -- perf \
  --root /path/to/repository --queries /path/to/queries.json --runs 30 --json
```

默认输出 Markdown 表格；`--json` 输出一个 JSON 文档。进度、跳过原因和采样警告写到 stderr。请用发布版测量性能，调试构建会在报告中标明。

## 质量评测

```text
solascope-bench quality --root DIR --queries FILE [--config TOML]
                       [--limit 10] [--no-index] [--json]
```

默认先增量更新项目索引。`--no-index` 复用已存在的索引；索引不存在、schema 或分块规则需要升级时先运行 `solascope index`。

查询文件格式与 `tests/fixtures/multilang/queries.json` 一致：

```json
[
  { "category": "symbol", "query": "readSession", "path": "cache.ts", "symbol": "readSession" },
  { "category": "error", "query": "TOKEN_CACHE_MISS", "path": "cache.ts", "symbol": "readSession", "start_line": 8, "end_line": 15 }
]
```

`path` 为项目相对路径；`symbol` 匹配完整符号，或限定名的最后一段（如 `Class.readSession`）。可选的行号是 1-based 闭区间。

运行以下可用模式，每个模式使用独立的 Engine 以避免跨模式的查询缓存污染：

| 模式 | 条件 |
| --- | --- |
| lexical | 总是可用 |
| hybrid，不重排 | 总是可用；Embedding 禁用时只有词法和符号 |
| semantic | 配置了 Embedding provider |
| hybrid+rerank | 配置了 Reranker；本地 ONNX 还需要 `local-rerank` 构建特性 |

未满足条件的模式列在 `skipped_variants` 中；已配置但请求失败的模式记录每条查询的错误。

### 指标

按模式、按查询类别分别报告：

- **Top1**：期望文件和符号排在第一位的查询比例。
- **Recall@5**：前五条内有期望文件和符号的查询比例。
- **MRR@10**：期望文件和符号在前十条的倒数排名均值；未命中取 0。
- **错误定位率**：返回了结果，但首条不在期望文件/符号，或其行范围与标注不同的查询比例。
- **无结果率**：搜索成功但没有结果的查询比例。
- **错误数**：搜索本身失败的查询数，也计入以上指标的总查询数，但不算作错误定位或无结果。

排名指标只判断文件和符号，错误定位率另外检查行号，因此可能出现 Top1 为 1 而错误定位率非零。每个模式还列出遗漏或错误的查询，以及单次搜索耗时的均值/P50/P95。

标注文件如果在源目录内，会从扫描和结果中排除，避免答案文件参与检索。使用 `--no-index` 时，应使用此前就未索引标注文件的索引，以免它们占用召回名额。

## 性能评测

```text
solascope-bench perf --root DIR [--queries FILE] [--config TOML]
                    [--runs 30] [--keep-index] [--json]
```

始终使用新临时索引目录，不读取或修改项目已有索引，也不改写源码。结束时删除临时索引；`--keep-index` 保留它，并在报告中给出路径。

测量：

1. 首次索引：完整扫描、解析、模型请求及持久化的墙钟耗时。
2. 无变化索引：立即再次执行增量扫描。
3. 显式重建：重新解析所有文件，复用可复用的精确向量缓存。
4. 每阶段的文件/Chunk/新 Embedding 数和索引目录内各文件大小。
5. 每阶段采样的进程峰值驻留内存。
6. 提供 `--queries` 时，按可用模式测量每条查询一次首次调用，再测量 `--runs` 次重复调用。

查询文件可以是上述标注对象数组，也可以是字符串数组。重复查询字符串仅测一次；未提供查询文件时跳过延迟测量。

**搜索计时不含进程启动**，包含 Engine 内的召回、融合、源码位置检查、可选模型请求和重排。首次时间单独报告；后续重复调用的最小值/P50/P95/最大值不包含首次调用。重复语义查询可能命中进程内向量缓存，因此不要把暖缓存时间当作模型推理延迟。

Linux 的内存采样读取 `/proc/self/status` 的 `VmHWM`（KiB），是整个进程的累计高水位，不是阶段独占内存；并发线程和内核近似计数可能造成小幅波动。其他平台明确报告不可用。

### 百分位

使用 nearest-rank：排序后取第 `ceil(p * n)` 个观测值。小样本的百分位仍会报告，但同时设置 `p50_undersampled` / `p95_undersampled` 并提示；例如 P95 在不足 20 个样本时就是最大值，不能据此推断生产延迟。

## 隐私和模型

内置默认配置禁用模型，可直接运行词法和符号评测。工具会读取用户及项目配置；比较不同模式时，可用 `--config` 固定模型设置。远程模型需要 `privacy.remote_opt_in = true`，密钥从配置同目录的 `.env` 或进程环境变量读取。首次临时向量索引没有缓存，会为每个 Chunk 请求 Embedding，运行前应确认费用和传输范围。

本地重排可使用：

```bash
cargo run --release -p solascope-bench --features local-rerank -- quality \
  --root /path/to/repository --queries /path/to/queries.json --config /path/to/config.toml
```

## 验证

```bash
cargo test -p solascope-bench
cargo clippy -p solascope-bench --all-targets -- -D warnings
```

测试覆盖指标计算、百分位、文件/符号/行范围匹配、错误与跳过处理、临时索引隔离和真实 CLI 输出结构。模拟响应只能验证流水线，不证明真实 Embedding 或 Reranker 的质量收益。
