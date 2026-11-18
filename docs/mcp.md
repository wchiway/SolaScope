# MCP 接入

SolaScope 提供 stdio 传输的 MCP 服务，与 CLI 共用同一套检索引擎和索引。

```bash
solascope mcp --root /abs/path/to/project
```

- 项目根在启动时固定，工具参数不能更改它。
- 服务是只读的：不建立、不更新索引。先运行 `solascope index`，或另开一个 `solascope watch` 保持索引更新。
- stdout 只输出协议消息，日志写到 stderr。
- 默认的返回条数、检索模式、路径过滤和是否重排取自配置，也可以在启动时用 `--limit`、`--mode`、`--path`、`--no-rerank` 指定。

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

`solascope` 不在 `PATH` 中时，把 `command` 换成可执行文件的绝对路径。需要远程模型的密钥时，通过客户端的 `env` 配置传入对应环境变量。

## 工具

### search_code

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `query` | string，必填 | 1–4096 字节 |
| `limit` | integer | 1–100 |
| `mode` | `lexical` \| `semantic` \| `hybrid` | |
| `path_filter` | string | 项目相对路径前缀或 glob |
| `rerank` | boolean | 是否使用已配置的 Reranker |

### find_symbol

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string，必填 | 符号名、限定名或其片段 |
| `kind` | string | 例如 `function`、`method`、`class`、`struct` |
| `path_filter` | string | |
| `limit` | integer | 1–100 |

### get_code_context

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `path` | string，必填 | 项目相对路径 |
| `start_line`、`end_line` | integer | 闭区间，1-based |
| `line`、`before`、`after` | integer | 中心行及前后行数，与区间写法二选一 |

最多返回 200 行、64 KiB。

### index_status

无参数。返回索引代际、文件数、Chunk 数、向量数、Embedding 指纹和最近更新时间。

## 返回结构

`search_code` 和 `find_symbol` 的 `structuredContent`：

```json
{
  "query": "INSUFFICIENT_STOCK",
  "mode": "hybrid",
  "index_version": 2,
  "warnings": [],
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

- `score` 只在一次查询内部可比较。
- `stale: true` 表示文件在索引后被修改或删除，行号可能已不准确，应重新索引。
- `match_sources` 取值为 `lexical`、`symbol`、`vector`、`rerank`。

同样的内容也以 JSON 文本放在 `content` 中，供不支持结构化结果的客户端使用。

## 错误

业务错误以 `isError: true` 的工具结果返回，服务不会退出：

```json
{ "error": { "message": "invalid input: paths must be project-relative and cannot escape the root", "exit_code": 2 } }
```

`exit_code` 与 CLI 退出码含义相同。参数类型不符或包含未知字段时，同样返回 `isError: true`，错误说明在 `content` 的文本中。
