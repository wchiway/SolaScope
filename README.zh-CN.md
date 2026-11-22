# SolaScope

[English](README.md) | 简体中文

本地优先的代码搜索工具，将自然语言问题、符号名、关键词或报错信息定位到文件、符号和代码行。可以通过 `solascope` / `sas` 命令行使用，也可以通过只读 MCP 服务接入助手。

SolaScope 无需模型即可运行。默认使用 SQLite FTS5 和符号检索，也可独立启用 Embedding 与重排。

- **增量索引**：只重新解析变化的文件，只为未命中缓存的输入请求向量。
- **源码定位**：结果包含相对路径、符号、行区间和片段；源码变化后标记 `stale`。
- **结构化解析**：使用 Tree-sitter 提取 Rust、Python、JavaScript/JSX、TypeScript/TSX 和 Go 定义，其他文本可按行分块。
- **独立模型配置**：Embedding 与重排分别选择服务，支持 Ollama、OpenAI 兼容接口、百炼 DashScope、硅基流动，以及可选的本地 ONNX 重排。
- **共用检索引擎**：CLI、MCP 和评测工具使用同一套索引与检索实现。

当前版本：**0.1.1**。已验证范围见[支持与限制](docs/implementation-status.md)。详细文档使用英文。

## 架构概览

![Hand-drawn overview: CLI and read-only MCP share an Engine that scans repository source, reads and writes a local SQLite index, and optionally calls embedding and reranking models.](docs/assets/solascope-overview.png)

CLI 与 MCP 共用同一引擎和索引。扫描与解析在本地完成，模型适配器按需启用。图中标注为英文，索引发布、缓存和一致性细节见[架构文档](docs/architecture.md)。

## 安装

### Windows：无需开发环境

在 PowerShell 或命令提示符中运行以下命令，直接执行[远程安装脚本](https://raw.githubusercontent.com/wchiway/SolaScope/main/install.ps1)：

```powershell
powershell -c "irm https://raw.githubusercontent.com/wchiway/SolaScope/main/install.ps1|iex"
```

脚本在 PowerShell 子进程中执行，无需保存安装脚本文件。脚本跟随远程 `main` 更新，程序从[已发布的 Release](https://github.com/wchiway/SolaScope/releases) 安装。

安装器面向 Windows 10/11 x64，使用系统自带 Windows PowerShell 5.1，无需 Rust、Git、Node.js、Python、包管理器或管理员权限。两个入口安装到 `%LOCALAPPDATA%\Programs\SolaScope\bin`，只更新用户 PATH，保留已有配置。在线模式需要访问 GitHub；离线 ZIP/校验文件安装、版本选择、升级及策略/验证边界见[安装指南](docs/installation.md)。安装后重新打开终端和 IDE。Windows 发行构建静态链接 CRT 并检查 DLL 导入，但干净机器上的实际运行仍需单独验证。

### Linux 和 macOS：预编译安装

直接执行[远程安装脚本](https://raw.githubusercontent.com/wchiway/SolaScope/main/install.sh)，无需保存安装脚本文件：

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/wchiway/SolaScope/main/install.sh | sh
# 可选：指定已发布版本和二进制目录。
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/wchiway/SolaScope/main/install.sh | sh -s -- --version 0.1.1 --bin-dir "$HOME/.local/bin"
```

上述命令执行远程 `main` 上的脚本；安装过程仍会下载已发布 Release 的二进制归档。

默认将 `solascope` 和 `sas` 安装到 `~/.local/bin`。脚本需要 POSIX shell、常用 Unix 工具、tar/gzip、SHA-256 工具，以及在线下载使用的 curl 或 wget；无需开发环境或 sudo。它校验下载哈希和暂存程序版本，支持离线安装、升级及失败回滚，只提示添加 PATH，不修改 shell 配置或已有应用配置。Linux 发行包要求 x86_64 glibc；macOS 可识别 Intel/Apple Silicon，但只能安装该 Release 实际提供的架构。平台限制与选项见[安装指南](docs/installation.md#linux-and-macos)。

### 自更新

```sh
solascope update
sas update --check
```

两个命令都会在当前可执行文件所在目录同时更新 `solascope` 和 `sas`。更新器获取 GitHub 最新正式 Release，验证 SHA-256 和暂存程序版本，保留配置及索引。不会降级到旧版本；`--force` 可重新安装同一版本。不调用 sudo 或远程安装脚本。Windows 更新前请关闭已有 MCP/watch 会话；旧程序仍在运行时，可能留下需要退出后清理的备份目录，命令会报告路径。详见[自更新说明](docs/installation.md#self-update)。

### 从源码安装

在本地源码目录安装两个 CLI 入口：

```bash
cargo install --path apps/solascope --locked
```

开发与验证使用 Rust 1.99，更早的工具链尚未测试。SQLite 随程序编译，无需数据库服务。Cargo 默认将两个入口安装到 `~/.cargo/bin`，请确保该目录在 `PATH` 中。

只构建、不安装：

```bash
cargo build --release --locked -p solascope-cli --bins
```

产物位于 `target/release/`。`solascope` 和 `sas` 的命令、参数及输出一致。

## 快速开始

在需要搜索的项目目录中运行：

```bash
solascope init
solascope index
sas search "where are database transactions committed"
sas status
```

`init` 创建用户配置，已有文件保持不变：

| 平台 | 用户配置 |
| --- | --- |
| Linux / macOS | `~/.solascope/.solascope.toml` |
| Windows（PowerShell） | `$env:USERPROFILE\.solascope\.solascope.toml` |

它不会创建项目配置，也不会将用户默认值绑定到当前项目。可以直接执行 `index` 使用内置默认值：默认递归扫描**执行命令时的当前目录及所有层级的子目录**，仍遵循忽略规则和隐私过滤。显式目录的优先级为命令位置参数、`--root`、进程环境变量 `SOLASCOPE__PROJECT__ROOT`；均未指定时使用当前目录，不自动扩大到上级 Git 根。

项目 `.solascope.toml` 用于提供设置，也可以从上级目录发现或用 `--config` 指定。配置文件所在位置及 TOML / 同目录 `.env` 中的 `project.root` 都不改变扫描目录。`project.index_dir` 只指定索引数据保存位置。搜索、状态和监听使用相同的目录选择规则。

创建可选的项目忽略文件：

```bash
solascope ignore
# 等价命令：sas ignore
```

命令在项目根目录创建 `.solascopeignore`，内容为注释形式的 gitignore 风格示例，不覆盖已有文件。使用 `ignore /path/to/project` 或 `ignore --root /path/to/project` 可以指定其他项目根目录。编辑该文件来忽略文件或文件夹；语法和隐私限制见[忽略文件说明](docs/configuration.md#ignore-files)。

开发时持续更新索引：

```bash
solascope index --watch
```

结果包含项目相对路径、从 1 开始的行区间、可用的符号名、分数和源码片段。源码修改后重新执行 `index`，或保持监听进程运行。

## 命令行

| 命令 | 用途 |
| --- | --- |
| `update [--check] [--force]` | 检查或安装 GitHub 最新正式版，同时更新两个入口 |
| `init [--probe]` | 创建用户配置；可选探测已配置的模型 |
| `ignore [path]` | 在项目根目录创建 `.solascopeignore`，不覆盖已有规则 |
| `index [path] [--rebuild] [--probe]` | 递归索引当前目录或显式路径；可选全量重建 |
| `index [path] --watch` | 首次索引后持续监听；`--no-watch` 覆盖 `index.watch` |
| `search <query>` | 搜索代码，可使用向量和重排 |
| `symbol <name> [--kind K]` | 按精确、前缀、包含或有限拼写纠错查找定义 |
| `context <path>[:line]` | 读取当前源码，最多 200 行或 64 KiB |
| `status` | 显示索引代际、数量、路径复用向量及模型指纹 |
| `watch [path] [--probe]` | 监听文件变化并增量索引 |
| `mcp [--probe]` | 启动只读 stdio MCP 服务 |
| `doctor [--models] [--prune-cache]` | 检查配置和索引一致性，探测模型或清理未引用缓存 |

通用参数包括 `--root`、`--config`、`--json`、`--limit`（1–100）、`--mode lexical|semantic|hybrid`、`--path`、`--no-rerank` 和 `--verbose`。

- `lexical` 使用 FTS5 召回。
- `semantic` 使用向量召回，需要配置 Embedding。
- `hybrid` 使用倒数排名融合（RRF）组合词法、符号和可选向量结果。

各检索模式均可独立启用重排。使用 `--json` 时，stdout 只输出一个 JSON 结果；警告和错误写 stderr，进度遵循日志配置。监听模式只将首次索引报告写到 stdout。

退出码：`0` 成功，`1` 内部、存储或 I/O 错误，`2` 参数或配置错误，`3` 索引缺失或过旧，`4` 模型失败或不兼容，`5` 写入者锁冲突，`130` 中断。

## 检索流程

![Hand-drawn query flow: query, mode-specific retrieval, RRF fusion, source checks, optional reranking, another source check, and deduplicated file, symbol, and line results.](docs/assets/solascope-search-flow.png)

检索模式决定启用哪些召回通道。源码检查会过滤被禁止的路径并标记过期代码，在重排后再次执行；重排只使用非过期候选，最终结果按实体去重。图中标注为英文，细节见[检索流水线](docs/architecture.md#search-pipeline)。

## 模型与配置

Embedding 和重排默认禁用，可通过用户配置或项目覆盖启用：

| 能力 | provider |
| --- | --- |
| Embedding | `ollama`、`openai`、`remote`、`dashscope`、`siliconflow` |
| 重排 | `remote`、`openai`、`dashscope`、`siliconflow`、`fastembed` |

除回环地址的 Ollama 外，HTTP 模型服务均要求 `privacy.remote_opt_in = true`。重排的 `openai` provider 使用供应商兼容请求格式，配置前需核对该服务的重排 API。本地 `fastembed` 重排需要用 `--features local-rerank` 编译，并准备兼容的 ONNX Runtime 和模型文件。

API Key 可放在配置文件同目录的 `.env` 中，例如 `DASHSCOPE_API_KEY` 和 `SILICONFLOW_API_KEY`。TOML 通过 `api_key_env` 引用变量名，已有进程环境变量优先。修改 `.env` 后应重启监听进程或 MCP 服务。

CLI 诊断日志同时追加到实际配置目录下的 `logs/solascope.log`。使用默认用户配置时，Linux/macOS 为 `~/.solascope/logs/solascope.log`，Windows 为 `$env:USERPROFILE\.solascope\logs\solascope.log`。日志记录脱敏后的重试状态和最终错误；设置 `log.persist = false` 可仅保留 stderr 输出。

全部字段、优先级、服务协议和示例见[配置文档](docs/configuration.md)。

向量索引绑定 provider、模型、endpoint、维度、归一化和输入模板组成的指纹。修改 Embedding 配置后重建索引：

```bash
solascope index --rebuild
```

更换重排模型或轮换密钥无需重建。解析器配置变化后，下次索引会重新解析；只读查询遇到过旧索引会提示升级，不会自动迁移。

## MCP 接入

在项目目录内建立索引并启动服务：

```bash
solascope index
solascope mcp
```

客户端配置示例：

```json
{
  "mcpServers": {
    "solascope": {
      "command": "solascope",
      "args": ["mcp"]
    }
  }
}
```

服务提供 `search_code`、`find_symbol`、`get_code_context` 和 `index_status`，并声明只读属性和输出 schema。启动时无需配置路径：工具默认使用服务进程的当前目录，agent 也可以在每次调用时通过可选 `root` 选择其他项目。索引默认位于各项目的 `.solascope/` 目录，无需重启即可读取独立 `index` 或 `watch` 进程发布的索引更新。上下文工具直接读取当前源码，无需预先建立索引。工具参数、返回结构和客户端接入见 [MCP 文档](docs/mcp.md)。

## 隐私

内置默认配置不使用模型，不向模型服务发送代码。启用远程 Embedding 或重排后，可能向已配置的服务发送索引中的代码片段。

源码读取限定在项目根内并遵循忽略规则。`.env`、私钥和凭据目录等敏感路径始终禁止索引和上下文读取。API Key 通过环境变量名引用，并在应用诊断中脱敏。endpoint 不允许携带凭据、查询串或 fragment；模型 HTTP 客户端不跟随重定向。

索引包含源码片段，默认位于项目内的 `.solascope/`。不要提交到版本控制或放入共享产物。完整隐私设置和排除路径见[配置文档](docs/configuration.md#privacy)。

## 文档与开发

以下详细文档均为英文：

- [配置](docs/configuration.md)
- [MCP 接入](docs/mcp.md)
- [架构](docs/architecture.md)
- [评测工具](benchmarks/README.md)与[实测数据](docs/benchmarks.md)
- [支持与限制](docs/implementation-status.md)

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --all-targets --locked --features local-rerank
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

GitHub Actions 已配置在 Linux、macOS、Windows 上执行工作区格式检查、Clippy、默认测试和打包测试，`local-rerank` 测试单独在 Linux 任务运行。所有检查通过后，可复用工作流构建两个 CLI 二进制，并连同公开文档及 `SHA256SUMS` 打包：Unix 使用 `.tar.gz`，Windows 使用 `.zip`，按版本和 Rust target 命名。工作流复用 Rust 依赖缓存，并取消被新提交替代的旧运行。

推送 `v0.1.1` 这样的版本 tag 后，工作流自动校验它与工作区版本一致且提交属于 `main` 历史，对该提交运行可复用的 CI/打包流程，并在全部资产验证通过后正式发布 **GitHub Release**。Unix 归档内包含 `install.sh`，Windows ZIP 内包含 `install.ps1`/`install.cmd`；三份安装脚本也作为独立资产发布。合并的 `SHA256SUMS` 覆盖三个归档和三份安装脚本，加上校验文件共七个资产。配置仓库 Secret `DEEPSEEK_API_KEY` 后，Description 通过 DeepSeek Responses 接口生成摘要，默认模型为 `deepseek-flash`；缺少密钥或接口失败时保留事实性提交记录，不阻断已验证的发行。已有 Release/tag 不会被覆盖。配置、按 tag 重试、隐私、预发布及验证边界见[发布操作说明](docs/releases.md)。未配置 crates.io 发布。

发行工具测试也可使用 Python 3.11 或更新版本在本地运行：

```bash
python -I -B -m unittest discover -s .github/scripts -p 'test_*.py' -v
```

Linux CLI 构建、自动化测试，以及本地构建的真实 Unix 发行包离线安装与重复安装已验证。Unix 下载流程和 macOS/Rosetta 架构检测使用 Linux 上的命令夹具测试；macOS 实际安装和公开 Release 下载仍待验证。Windows 安装器离线测试已使用 Windows PowerShell 5.1 执行，未修改真实用户 PATH。模型协议和 DeepSeek Description 测试使用本地夹具/模拟接口。真实模型质量、本地 ONNX 推理、干净机器上的 macOS/Windows CLI 发行版运行、商业 MCP 客户端、远程 CI 发行包、真实 DeepSeek 生成以及 GitHub Release 正式发布仍待验证。

## 许可证

[Apache License 2.0](LICENSE)。
