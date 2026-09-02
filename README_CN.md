<h1 align="center">Stravia AI Gateway</h1>

<p align="center">
  本地运行、可自托管的 AI 网关，让 OpenAI、Anthropic 和 Gemini 客户端连接到你选择的模型提供商。
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

> **项目状态：** Stravia 当前版本为 `0.1.0`，仍在积极开发中。稳定版本发布前，配置格式和数据库兼容性可能发生变化。

## 项目简介

Stravia 运行在 AI 客户端与上游模型提供商之间。客户端继续使用自身支持的协议；Stravia 负责解析虚拟模型、选择上游后端、在必要时转换请求与响应，并在本地记录调用结果。

```text
Claude Code · Codex CLI · Gemini CLI · OpenCode · 各类 SDK
                            │
                            ▼
              Stravia 统一监听端口 :23471
                ├─ OpenAI 兼容 API
                ├─ Anthropic Messages API
                ├─ Gemini GenerateContent API
                ├─ Admin API
                └─ WebUI
                            │
                            ▼
 OpenAI · Anthropic · Google · Vertex AI · DeepSeek · Ollama · …
```

同一套 Rust 核心支持两种部署形态：

- **桌面应用：** 基于 Tauri 的本地网关与管理界面。
- **独立服务端：** 单个二进制通过统一端口提供代理 API、Admin API、健康探针和内嵌 WebUI。

## 当前能力

### 协议网关

| 客户端协议                | 端点                                                   |
| ------------------------- | ------------------------------------------------------ |
| OpenAI Chat Completions   | `POST /v1/chat/completions`                            |
| Open Responses 2026-04-24 | `POST /v1/responses`（JSON、SSE、WebSocket）           |
| OpenAI Embeddings         | `POST /v1/embeddings`                                  |
| Anthropic Messages        | `POST /v1/messages`                                    |
| Gemini GenerateContent    | `POST /v1beta/models/{model}:generateContent`          |
| Gemini 流式生成           | `POST /v1beta/models/{model}:streamGenerateContent`    |

Stravia 支持 JSON、SSE 与 Open Responses WebSocket 交付、跨协议工具调用、推理内容、用量数据，以及上游无需修改时的同协议透传。

隐藏的 Platform Tool 续跑通过 HTML comment 形式的 History Marker 投影到客户端历史。OpenAI-compatible Chat Completions 在首个非空 `content` delta 前继续通过 `reasoning_content` 交付 Thinking；当上游在 item 开始时明确标记 protected reasoning，其公开 summary delta 会保持实时交付，opaque 字节仍只保存在 Marker 后。之后的 Thinking 通过 `content` 以 Markdown 引用 Preview 流式交付，后续 Thinking Marker 与 Platform Marker 也使用 `content`，从而在客户端按字段聚合时保持顺序。纯文本客户端可能直接显示这些 Marker comment。Open Responses、Anthropic Messages 与 Gemini 保留原生有序 reasoning/thinking carrier；若所选协议无法表示已观察到的顺序，Stravia 会显式失败，而不会延迟普通 Text。

重新提交完整历史的客户端必须原样保留 History Marker 与 Projection Delimiter。Stravia 会删除仅用于展示的 Preview 字节，并在原位置恢复权威 Thinking、ToolCall 与 ToolResult；删除 Marker 或 Delimiter 会被视为有意编辑历史。客户端关闭流式传输时，Stravia 会先执行仅含 Platform Tool 的隐藏续轮，再一次性返回语义等价的 buffered projection。live stream 则在启动对应 Platform Tool 前交付并发布每个 Marker。

OpenAI direct 与 Codex OAuth 的生成 Target 会为 Chat Completions、Open Responses、Anthropic Messages 和 Gemini 请求使用上游 Responses WebSocket，不受客户端是否流式影响；Embeddings 仍只使用 HTTP。Hook 与协议可表示性检查完成后，Stravia 可从最长且严格等价的 canonical item 前缀续接；Principal、精确 Target、Provider 账号与配置、resolved model、instructions、tools、reasoning、response format 和请求控制必须全部一致。任一条件不匹配都会发送完整有效历史，不会削弱请求语义。

`POST /v1/responses` 以 Open Responses 2026-04-24 作为 canonical baseline，同时接受结构安全的 rolling additive 字段和 hosted tool 声明。同协议 Target 保留这层 compatibility envelope；跨协议 Target 可以省略 advisory 字段和未被强制选择的 hosted tools，但绝不省略内容或硬约束。`POST /v1/responses/compact` 已被识别，但固定返回 `unsupported_feature`；后台执行仍不支持。

### 提供商与模型路由

当前内置的提供商元数据包括：

- OpenAI 与 Codex OAuth 通道
- Anthropic 与 Claude Code OAuth 通道
- Google Gemini 与 Vertex AI
- DeepSeek、Moonshot AI、Zhipu AI、Z.AI、MiniMax、xAI（API Key 与 Grok OAuth）和 NVIDIA
- OpenRouter、Ollama 以及自定义 OpenAI 兼容端点

客户端发送一个 **Model ID**。该值就是 Route ID，匹配时包含字母大小写在内完全精确。逻辑 Model 还可以设置可选、可重复的展示名称；展示为空时回退到 Model ID，并且永不参与路由、授权或绑定。对应 Route 可以包含一个或多个 Target，并使用 weighted、priority、cooldown 或 latency 选择策略。Stravia 从 revisioned `models.stravia.cn` 索引刷新 Provider Catalog：轻量 Provider 与 Canonical Model 索引以同一 revision 原子更新，Provider-scoped inventory 仅在需要时加载。Catalog Provider 使用其 scoped inventory；账号级 discovery 仍决定可调用的模型 ID，Core 只为精确匹配补充元数据，不会加入仅存在于 Catalog 的模型。

添加提供商时，先选择完整的提供商/通道选项。API Key 与 OAuth 通道是独立选项，创建后不能互相转换。Codex 与 Claude Code OAuth 在桌面端和通过回环地址访问的 WebUI 中会自动接收回调；远程 WebUI 则会在浏览器登录后要求粘贴完整 callback URL。Grok OAuth 使用 xAI device authorization flow：WebUI 打开验证页面，在需要时显示 user code，并轮询直到授权完成。

WebUI 为每种资源保留唯一编辑表面。添加或编辑逻辑 Model 时，Model ID 组合框可以按名称或 ID 搜索 Canonical Model，并在目录不可用时继续接受自定义 ID；选择模板会复制其展示名称，两个字段都可继续编辑。手动 Provider Model 仍可搜索 Canonical Model 模板；选择不会创建 Backend，也不会保存隐藏 binding。新 Provider 保存后会进入详情页并开始同步 Provider Model；详情视图分别管理连接设置、持久化 Provider Model 清单和 Route 引用。Provider Model metadata 在独立抽屉中保存，Selection Policy 则立即生效，并且只控制新 Target 候选的 Effective Availability。Provider Model 变为不可用不会改写已有 Route Target。管理员可以从精确 Provider Catalog Entry 显式 re-import 已发现的 Provider Model；普通同步不会覆盖本地 metadata。

连接页面生成基于 Open Responses 的 OpenCode provider。Claude Code 配置会在所选默认 Route 声明的能力可由 Claude Code 表达时，自动写入 `effortLevel` 和 `autoCompactWindow`。

Route Builder 使用独立页面。选择 Provider 后会自动加载其可用 Provider Model；如需绑定清单外的 upstream model ID，必须显式进入未经验证的自定义分支。weighted Route 显示相对流量比例，priority Route 使用可见顺序和支持键盘的上移、下移操作。删除 Provider 时会在同一事务内移除其 Target、删除由此变空的 Route，并保留仍有其他 Target 的 Route。

### 联网搜索与 MCP

可选的联网搜索只公开一个 `web_search` 能力，返回终态、带来源的 Search Report，而不是单页搜索结果。成功结果包含答案、已引用的公网 HTTP(S) 来源、限制、完成状态、用量和稳定 `turn_id`。将该 ID 作为 `previous_turn_id` 传入，可从同一 Principal 的完整祖先链续接或创建独立分支；Stravia 不会隐式选择“最新”Turn。

在 WebUI 中配置一个 Search Backend。Local Search 使用有界 Agent 编排有序的内部 Web Access Search/Fetch 来源：自动创建的进程内 Local Provider、Exa 或智谱。每个 Web Provider 都可独立选择是否使用 Gateway 代理。Codex Agentic Search 固定到一个精确且兼容的 Codex OAuth Responses Provider/model，不使用 Local budget。Local 与 Codex 之间不做 fallback。

平台联网搜索总开关统一控制所有有效 API Key 的显式访问。每个 Key 分别控制 MCP 访问和透明注入；透明注入只把所选且已启用的能力加入兼容请求，不限制显式调用或 MCP。MCP 客户端连接 `POST /mcp`，通过 `Authorization: Bearer <key>` 认证，并且只在 MCP 权限与平台能力都开启时发现 `web_search`。OpenAI Responses 的原生 web-search 声明与隐藏 tool continuation 使用同一个 Search contract。

联网搜索与 Web Access 配置属于部署本地状态，不参与配置导出/导入。Search Turn 只保留 Report 元数据与引用 URL，不保存抓取的网页正文或内部 Agent transcript。

### Media Understanding

Media Understanding 公开一个用于静态 JPEG、PNG 与 WebP 图片的 `understand_media` 能力。若父 Route 存在支持图片的 Target，Stravia 会原样发送图片；否则，支持工具的父 Model 可调用已配置的隐藏视觉 Model，并获得包含 source ArtifactId 与可分支 `turn_id` 的强校验 Media Report。原生视觉 Route 失败后不会 fallback 到隐藏 Model。

在**多模态理解**页面启用平台能力、选择逻辑 Model 并设置思考等级。选择器只列出所有 Target 都明确声明图片输入能力的已启用 Model；思考等级选择器只列出每个 Target 都支持的等级。启用后，所有有效 API Key 都能显式调用 `understand_media`；MCP 访问和透明注入仍由每个 Key 独立控制。隐藏调用计入调用方配额，但不会授予所选 Model 的直接访问权。外部图片 URL 仅允许公网 HTTPS 目标，并会在使用前创建 snapshot。预处理始终生成有界的有损 JPEG derivative，忽略 ICC profile，因此精确颜色或细小文本 OCR 可能不准确。

### 本地管理

SvelteKit WebUI 可管理：

- 提供商、认证、模型发现和连通性检查
- 虚拟模型及其上游后端
- 可自动生成或自定义并编辑完整密钥的 API Key、模型绑定、有效期、Principal Concurrency Limit 和执行权限
- 请求日志、延迟和 Token 用量统计
- 在**额度总览**矩阵中查看 Provider 上报的配额、请求额度和余额，并按条件筛选、查看重置时间轴及基于 30 分钟采样的当前窗口耗尽预报；现场读取仍支持三分钟缓存、单个 Provider 刷新，并在刷新失败时保留上次成功结果
- 运行时设置
- SDK 与 AI 编码工具的可复制集成示例

界面支持英文与简体中文、响应式导航，以及浅色、深色和跟随操作系统三种主题。首次使用时，简体中文（`Hans`）客户端 locale 会选择 `zh-CN`，不支持的 locale 使用英文；可在 Login 页面或**设置 → 外观**中无刷新切换语言，每个浏览器或桌面 WebView 分别记住自己的选择。

### 存储与部署

- **SQLite** 是默认存储后端。
- **PostgreSQL** 提供持久化存储。
- SQLx migrations 会在监听器启动前执行。
- `GET /healthz` 是存活探针；`GET /readyz` 用于报告存储就绪状态。

`0.1.0` 当前仅支持新建的 SQLite 和 PostgreSQL 数据库，不提供旧 schema 的就地升级路径。

## 发布版本

版本 tag 会通过 [GitHub Releases](https://github.com/Stravia-AI/StraviaPlatform/releases) 发布 Server 压缩包和 Desktop 安装包，同时发布多架构容器镜像和 Nix package。当前发布范围：

- Server：Linux 和 Windows 的 x86_64、ARM64 架构；Linux 同时提供 GNU 与 musl 压缩包。
- Desktop：Linux AppImage 和 Windows NSIS 安装包，均覆盖 x86_64 与 ARM64。
- 容器：`ghcr.io/stravia-ai/straviaplatform` 下的 `linux/amd64` 与 `linux/arm64`。
- Nix：仓库 flake 提供原生 `x86_64-linux` 和 `aarch64-linux` package，release 构建会推送到 [`stravia-platform` Cachix cache](https://app.cachix.org/cache/stravia-platform)。

当前不提供 macOS 产物。Linux GNU Server 压缩包和 Desktop AppImage 以 Ubuntu 24.04 为兼容基线；旧版 Linux 发行版应使用 musl Server 压缩包。Windows Desktop 安装包暂未签名，可能触发 Microsoft Defender SmartScreen。

`SHA256SUMS` 列出了所有可下载构建产物。运行二进制或安装包前请先校验：

```bash
sha256sum path/to/downloaded-asset
```

将结果与 `SHA256SUMS` 中对应文件的记录对比。Windows 用户可运行 `Get-FileHash C:\path\to\downloaded-asset -Algorithm SHA256`。

维护者应使用 `vMAJOR.MINOR.PATCH` 或 SemVer 预发布 tag；版本必须与 `Cargo.toml`、`package.json`、`tauri.conf.json` 一致，且 tag 对应 commit 必须属于 `master`。稳定版会更新完整版本、次版本、主版本和 `latest` 镜像 tag；预发布版只更新完整版本 tag。首次推送 GHCR 时创建的 package 默认为 Private，维护者需要在 package settings 中将可见性一次性改为 **Public**。

## 从源码快速开始

### 环境要求

- Rust `1.97.1`
- Bun `1.4.0`
- [Task](https://taskfile.dev/) `3.52.0`
- Python E2E 测试需要 uv `0.11.28`
- 构建桌面应用时需要 Tauri 对应平台依赖

### 运行独立服务端

```bash
# 使用 Vite WebUI 的开发模式
task dev:server

# 构建内嵌 WebUI 和 release 服务端二进制
task build:server

# macOS / Linux
./target/release/stravia-server

# Windows
.\target\release\stravia-server.exe
```

默认配置使用 SQLite，并监听 `127.0.0.1:23471`。Debug 构建与桌面端共用仓库内的 `.stravia-dev/` 数据目录；Release 构建使用 `~/.stravia`。打开 <http://127.0.0.1:23471>，配置提供商并创建模型路由。

### 使用 Nix 运行服务端

```bash
# 当前 checkout
nix run .

# 已发布 tag；将 vX.Y.Z 替换为所需版本
nix run github:Stravia-AI/StraviaPlatform/vX.Y.Z
```

flake 支持 `x86_64-linux` 和 `aarch64-linux`，会把内嵌 WebUI 与 Server 构建为一个 package，并将公开的 `stravia-platform` Cachix cache 配置为 substituter。

在 NixOS 中，可以从 flake 导入 service module：

```nix
{
  inputs.stravia.url = "github:Stravia-AI/StraviaPlatform";

  outputs = { nixpkgs, stravia, ... }: {
    nixosConfigurations.gateway = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        stravia.nixosModules.default
        {
          services.stravia.enable = true;
        }
      ];
    };
  };
}
```

service 默认监听 `127.0.0.1:23471`，使用动态系统用户运行，并将数据持久化到 `/var/lib/stravia`。如需对外提供服务，请配置 `services.stravia.host`、`port` 和 `openFirewall`。密钥及其他可选服务端设置应放入 `services.stravia.environmentFile`；监听非 loopback 地址时必须设置 `STRAVIA_ADMIN_TOKEN`。

### 使用 Docker 运行服务端

```bash
# 拉取最新稳定版多架构镜像
docker pull ghcr.io/stravia-ai/straviaplatform:latest

docker run --rm \
  --publish 23471:23471 \
  --env STRAVIA_ADMIN_TOKEN=replace-with-a-long-random-token \
  --mount source=stravia-data,target=/data \
  ghcr.io/stravia-ai/straviaplatform:latest
```

如需从当前 checkout 构建，请运行 `docker build --tag stravia-server:local .`，并把最后的镜像名替换为 `stravia-server:local`。镜像内嵌生产 WebUI，监听 `0.0.0.0:23471`，以非 root 用户运行，并把 SQLite 数据持久化到 `/data`。由于容器监听地址不是 loopback，必须设置 `STRAVIA_ADMIN_TOKEN`。内置健康检查会请求 `GET /healthz`。

创建名为 `my-model` 的虚拟模型后，可以通过任意受支持协议调用：

```bash
curl http://127.0.0.1:23471/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_PROXY_KEY" \
  -d '{
    "model": "my-model",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

只有当所选模型路由受到 API Key 保护时，才需要携带 Authorization 请求头。

### 运行桌面应用

```bash
# 开发模式
task dev:desktop

# 生产构建
task build:desktop
```

开发构建会把服务端和桌面端运行状态（包括 `gateway.db` 和桌面端固定端口配置）统一放在仓库根目录下已忽略的 `.stravia-dev/` 目录中。Release 服务端使用 `~/.stravia`；Release 桌面端仍使用操作系统的应用数据目录。

`task dev:server` 和 `task dev:desktop` 会启用仅限 Debug 构建的 wire capture，并写入 `.scratch/wire-captures/`。即使敏感请求头值已脱敏，录制文件仍包含完整请求体和响应体；请仅在本机保留，并在诊断后删除。

桌面进程会在 `127.0.0.1` 上启动同一个统一 HTTP 应用。首次使用时，应用优先绑定默认固定端口 `23471`；后续启动会优先使用在 **设置 → 桌面端** 保存的固定端口。若首选端口无法绑定，Stravia 仍会使用临时随机端口保持可用，在概览页报告冲突，并允许用户无需重启即可重新检测或更换固定端口。此桌面本机设置不会改变下方独立服务端的参数。

## 服务端配置

常用命令行参数和环境变量：

| 命令行参数               | 环境变量                       | 默认值       |
| ------------------------ | ------------------------------ | ------------ |
| `--host`                 | `STRAVIA_HOST`                 | `127.0.0.1`  |
| `--port`                 | `STRAVIA_PORT`                 | `23471`      |
| `--public-origin`        | `STRAVIA_PUBLIC_ORIGIN`        | 未设置       |
| `--admin-token`          | `STRAVIA_ADMIN_TOKEN`          | 未设置       |
| `--data-dir`             | `STRAVIA_DATA_DIR`             | Debug：`.stravia-dev`；Release：`~/.stravia` |
| `--storage-backend`      | `STRAVIA_STORAGE_BACKEND`      | `sqlite`     |
| `--postgres-dsn`         | `STRAVIA_POSTGRES_DSN`         | 未设置       |
| `--log-level`            | `STRAVIA_LOG_LEVEL`            | `info`       |
| `--config-poll-interval` | `STRAVIA_CONFIG_POLL_INTERVAL` | `3` 秒       |
| `--wire-capture-dir`¹    | `STRAVIA_WIRE_CAPTURE_DIR`     | 未设置       |

¹ 仅 Debug 构建提供。启用后，Stravia 为每个请求写入一份关联 JSONL，包含客户端和上游的请求与响应。敏感请求头值会替换为 `***`；请求体和响应体以可读 UTF-8 文本保存到 `body` 字段，可能包含提示词、工具数据、媒体引用和模型输出。录制文件应仅保留在本机，并在诊断后删除。Release 构建不包含该参数和录制实现。

可将录制的上游响应帧交给真实协议 decoder 回放：

```powershell
$env:STRAVIA_WIRE_REPLAY_FILE = ".scratch/wire-captures/req-....jsonl"
cargo test -p stravia-core replay_wire_capture_from_environment -- --ignored --nocapture
```

使用 OpenAI Images `response_format=url` 或 MCP 图片 resource link 前，须通过 `--public-origin` 配置可信且可从外部访问的 Gateway origin（例如 `https://gateway.example.com`）。Stravia 不会从请求转发头推导签名 Artifact URL。

服务监听非回环地址时，必须设置 admin token：

```bash
./target/release/stravia-server \
  --host 0.0.0.0 \
  --admin-token YOUR_ADMIN_TOKEN
```

使用 PostgreSQL：

```bash
./target/release/stravia-server \
  --storage-backend postgres \
  --postgres-dsn "postgres://user:pass@localhost:5432/stravia"
```

## 开发

```text
backend/crates/stravia-core/       与传输层无关的网关、协议、提供商、存储和管理服务
backend/crates/stravia-devtools/   协议样本录制与回放工具
backend/apps/stravia-server/       独立统一 HTTP 服务端
backend/apps/stravia-desktop/      Tauri 桌面外壳
frontend/stravia-webui/            SvelteKit 管理界面
tests/e2e/                         Python 后端 E2E 套件与协议录制样本
```

常用命令：

| 命令                     | 用途                                                 |
| ------------------------ | ---------------------------------------------------- |
| `task dev:web`           | 启动 WebUI 开发服务器                                |
| `task dev:server`        | 启动 Vite WebUI 和 debug 独立服务端                  |
| `task dev:desktop`       | 以开发模式启动 Tauri 桌面应用                        |
| `task check`             | 运行 WebUI 检查、ESLint、Rust 格式和 Cargo 检查      |
| `task test`              | 运行 WebUI 和受支持的 Rust 单元测试                  |
| `task test:e2e:web`      | 运行 Chromium WebUI E2E 测试                         |
| `task test:e2e:desktop`  | 运行 Windows Tauri/WebView2 冒烟测试                 |
| `DB_URL=… task test:e2e` | 运行完整 Proxy、Admin、SQLite 和 PostgreSQL E2E 套件 |

后端 Python 测试使用 `pyproject.toml` 中锁定的 `test` 依赖组，Task 通过 `uv run --locked` 执行。
Debug 服务端构建不会内嵌或提供 WebUI 资源。`task dev:server` 会同时启动 Vite 开发服务器和后端；Release 服务端构建仍会内嵌 WebUI。

## 文档

- [架构设计](docs/design/architecture.md)
- [数据库结构](docs/database/schema.md)

## 许可证

Stravia 采用 [GNU Affero General Public License v3.0 only](LICENSE)（`AGPL-3.0-only`）许可。
单独许可的组件和资源继续适用其各自许可证文件中的条款，包括采用 `CC0-1.0` 的 `stravia-web-access` crate，以及采用各自许可证的内置字体。
