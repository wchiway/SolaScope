# MCP 接入

SolaScope 提供 stdio 传输的 MCP 服务，与 CLI 共用同一套检索引擎和索引。

```bash
solascope index --root /abs/path/to/project
solascope mcp --root /abs/path/to/project
```

- 项目根在启动时固定，工具参数不能更改它；每个实例服务一个项目。
- 服务是只读的：不建立、不更新、不迁移索引。先运行 `index`，或另开 `watch` / `index --watch` 保持索引更新。
- stdout 只输出协议消息；诊断默认写 stderr，也可通过 `log.output` 指向受保护的项目内日志文件。
- 默认返回条数、检索模式、路径过滤和是否重排取自配置，也可在启动时用 `--limit`、`--mode`、`--path`、`--no-rerank` 指定。
- 索引不存在或 schema/分块规则需要升级时，工具返回可操作错误，不自动触发昂贵重建。

## 客户端配置

多数客户端使用如下 JSON 结构：

```json
{
  "mcpServers": {
    "solascope": {
      "command": "solascope",
      "args": ["mcp", "--root", "/abs/path/to/project"]
    }
  }
}
```

Claude Code：

```bash
claude mcp add solascope -- solascope mcp --root /abs/path/to/project
```

`solascope` 不在 `PATH` 中时，把 `command` 换成可执行文件的绝对路径，使用 `sas` 也等价。远程模型密钥可保存在用户配置同目录的 `.env`（Linux/macOS：`~/.solascope/.env`；Windows：`$USERPROFILE/.solascope/.env`），也可通过客户端 `env` 传入进程变量，后者优先。`--config FILE` 另会读取 FILE 同目录的 `.env`。修改文件后需重启 MCP；不要把密钥放到参数、URL 或 TOML 中。远程模型仍要求 `privacy.remote_opt_in = true`，Embedding 和 Reranker 独立配置，详见[配置](configuration.md)。

客户端接入示例不代表已在所有商业客户端实测。当前端到端验证使用测试内的原生 JSON-RPC 客户端和 rmcp SDK 客户端。

### 可选启动探测

```bash
solascope mcp --root /abs/path/to/project --probe
```

`--probe` 使用固定句子检查配置模型，不读取源码作为探测输入；可能产生模型调用费用。本地模型也需预先具备运行环境。探测失败时不会进入正常服务；探测报告只写 stderr，不污染 JSON-RPC stdout。未指定时不隐式进行启动推理请求。

## 工具

### search_code

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `query` | string，必填 | 1–4096 UTF-8 字节，不能为空白 |
| `limit` | integer | 1–100 |
| `mode` | `lexical` \| `semantic` \| `hybrid` | 不指定则用服务默认值 |
| `path_filter` | string | 项目相对路径前缀或 glob |
| `rerank` | boolean | 是否使用已配置的 Reranker |

默认模型禁用时，`hybrid` 只使用词法加符号，并给出警告；`semantic` 返回模型不可用错误。允许的 hybrid 查询在模型临时不可用或认证失败时降级，模型指纹/维度不兼容不降级。Reranker 失败按 `rerank.fail_open` 保留融合排序或返回错误。

### find_symbol

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string，必填 | 符号名、限定名或其片段，1–4096 字节 |
| `kind` | string | 例如 `function`、`method`、`class`、`struct` |
| `path_filter` | string | 项目相对路径前缀或 glob |
| `limit` | integer | 1–100 |

匹配优先级为精确、前缀、包含、有限拼写纠错。拼写纠错结果的 `match_sources` 为 `fuzzy`，仅用于此工具。该工具不调用 Embedding 或 Reranker。

### get_code_context

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `path` | string，必填 | 项目相对路径 |
| `start_line`、`end_line` | integer | 闭区间，1-based |
| `line`、`before`、`after` | integer | 中心行及前后行数，与区间写法二选一 |

最多返回 200 行、64 KiB。读取当前源文件而不是索引中的历史正文，受相同忽略和敏感路径规则约束。拒绝 `..`、绝对路径、Windows 盘符/UNC 逃逸和指向项目根外的符号链接；include 规则不能解除敏感路径禁止。

### index_status

无参数（调用参数为 `{}`）。返回项目 ID、固定根目录和索引目录、索引代际、文件/Chunk/向量数、缓存条目数、`relocated_vectors`、Embedding 指纹、最近更新时间和未完成任务数。

`relocated_vectors` 表示移动/重命名文件时复用了旧路径的向量。显式 `index --rebuild` 会按当前完整输入重新计算这些近似复用项。该工具不探测模型、不更新索引。

## 返回结构

`search_code` 和 `find_symbol` 的 `structuredContent` 形如以下示例；路径、分数和耗时仅用于展示结构：

```json
{
  "query": "INSUFFICIENT_STOCK",
  "mode": "hybrid",
  "index_version": 2,
  "warnings": [],
  "timings": {
    "embedding_ms": 0.0,
    "retrieval_ms": 1.2,
    "rerank_ms": 0.0,
    "total_ms": 1.2
  },
  "results": [
    {
      "project_id": "example-project",
      "relative_path": "inventory.go",
      "symbol": "Inventory.Reserve",
      "kind": "method",
      "language": "go",
      "signature": "func (inventory *Inventory) Reserve(sku string, quantity int) error",
      "parent_symbol": "Inventory",
      "start_line": 13,
      "end_line": 20,
      "snippet": "...",
      "score": 0.0164,
      "rank": 1,
      "match_sources": ["lexical"],
      "index_version": 2,
      "stale": false
    }
  ]
}
```

- `score` 用于同一次查询内的排序，跨模型或跨模式时数值不可直接比较。
- `stale: true` 表示文件在索引后被修改、删除或读取时发生变化，行号可能已不准确，应重新索引；删除文件的片段为空。被当前隐私规则屏蔽的结果直接移除。
- `match_sources` 可包含 `lexical`、`symbol`、`vector`、`rerank`；`find_symbol` 的拼写纠错使用 `fuzzy`。
- `timings` 单位为墙钟毫秒，`retrieval_ms` 包括本地召回、融合、源码校验等，不只计 SQL。未执行或命中查询向量缓存的模型阶段为 0；总耗时不包含客户端启动进程的时间。
- 查询使用完整的 SQLite 读事务快照，同次返回的代际一致；重排等待结束后再次核对源码位置。

同样的内容也以 JSON 文本放在 `content` 中，供不支持结构化结果的客户端使用。请求取消会中断等待并丢弃结果；已经在阻塞线程中运行的 ONNX 推理不能被强制抢占。

## 错误与修复

业务错误以 `isError: true` 的工具结果返回，服务不会因为单次搜索失败而退出：

```json
{ "error": { "message": "invalid input: paths must be project-relative and cannot escape the root", "exit_code": 2 } }
```

| `exit_code` | 常见原因与处理 |
| --- | --- |
| 1 | 存储或 I/O 错误，运行 `doctor` 检查 |
| 2 | 参数、配置或隐私边界错误，修正请求或配置 |
| 3 | 索引不存在、schema 或分块规则过旧，另行运行 `index` |
| 4 | 模型不可用、认证失败或向量空间不兼容；检查服务/配置，指纹改变后运行 `index --rebuild` |
| 5 | 写入者冲突，通常发生在另行运行的 `index`/`watch` 命令 |
| 130 | 操作被取消 |

参数类型不符或包含未知字段时，同样返回 `isError: true`，错误说明在 `content` 的文本中。API Key 和完整远程错误正文不会作为工具错误回显。

索引维护和诊断通过 CLI 的 `index`、`watch`、`doctor` 完成。MCP 每个实例固定服务一个项目，提供上述四个工具。测试与限制见[支持范围](implementation-status.md)。
