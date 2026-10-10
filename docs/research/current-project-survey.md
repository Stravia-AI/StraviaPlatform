# 当前项目调查

日期：2026-10-10。范围：StraviaPlatform 当前仓库的产品边界、架构、前后端实现与验证工作流。

本报告汇总四路只读 subagent 调查（ArchitectureSurvey、BackendSurvey、FrontendSurvey、WorkflowSurvey）以及主 agent 的本地 CLI 冒烟。未修改业务代码、未运行构建或测试、未调用模型上游或生产服务。报告不是完整代码评审、安全审计或当前部署实例的健康证明。

## 结论

Stravia 是本地运行、可自托管的 Agent 基础设施，协议网关只是模型接入层。产品还包含平台工具、程序内置且版本化的 Agent Definition、访问控制、历史、用量与诊断；不是用户自定义 Agent 或可视化工作流构建器。[产品说明](../../README_CN.md)（30–54 行）、[架构定位](../design/architecture.md)（7–11 行）。

当前 workspace 版本为 `0.4.0`，Rust edition 为 `2024`，Rust 工具链锁定 `1.98.1`，Bun 锁定 `1.4.2`。Cargo 默认成员仅为 Server，裸 Cargo 命令不等于完整 workspace 检查。[Cargo 清单](../../Cargo.toml)（1–11 行）、[Rust 工具链](../../rust-toolchain.toml)（1–4 行）、[Bun 配置](../../package.json)（5–9 行）。本轮实际 `rustc --version` 与 `bun --version` 和这两个锁定版本一致。

当前可证实的主要问题是架构文档漂移；已有性能报告还记录了未达标与诊断投影缺口，不能据此宣称当前所有路径稳定或性能验收通过。后者是历史报告证据，本轮没有重新测量。

## 模块地图

| 模块 | 当前职责与依赖边界 | 依据 |
| --- | --- | --- |
| `stravia-runtime-contract` | canonical IR、运行能力和共享执行契约 | [crate 清单](../../backend/crates/stravia-runtime-contract/Cargo.toml)（1–21 行） |
| `stravia-protocol-codec` | 标准协议编码、解码和 canonical 转换；依赖 runtime-contract | [crate 清单](../../backend/crates/stravia-protocol-codec/Cargo.toml)（12–19 行） |
| `stravia-vendor-sdk` / `stravia-vendor-runtime` | Guest SDK/WIT 与 Wasmtime Component 宿主执行 | [SDK 清单](../../backend/crates/stravia-vendor-sdk/Cargo.toml)（12–20 行）、[Runtime 清单](../../backend/crates/stravia-vendor-runtime/Cargo.toml)（12–23 行） |
| Vendor guest crates | 默认内嵌 base；七个 dedicated 组件由全量构建入口产生 | [构建清单](../../backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs)（10–19、32–38 行） |
| `stravia-core` | Gateway、路由、模型执行、平台工具、内置 Agent、持久化与管理业务 | [依赖清单](../../backend/crates/stravia-core/Cargo.toml)（15–24 行）、[模块入口](../../backend/crates/stravia-core/src/lib.rs)（1–40 行） |
| `stravia-server` | 共享 HTTP application、独立服务进程、可选 WebUI 嵌入 | [HTTP application](../../backend/apps/stravia-server/src/lib.rs)（78–108 行）、[特性配置](../../backend/apps/stravia-server/Cargo.toml)（13–27 行） |
| `stravia-desktop` | Tauri 壳、同一 Core/HTTP application 的生命周期、原生管理会话与本地 IPC | [实际组装](../../backend/apps/stravia-desktop/src/lib.rs)（468–545 行）、[依赖](../../backend/apps/stravia-desktop/Cargo.toml)（15–29 行） |
| `frontend/stravia-webui` | Server/Desktop 共用的静态管理客户端 | [依赖](../../frontend/stravia-webui/package.json)（14–84 行）、[静态 adapter](../../frontend/stravia-webui/svelte.config.js)（1–14 行） |

Core 不拥有 HTTP listener；Desktop 复用 Server crate 的 HTTP application，而不是再实现一套管理业务。[架构约束](../design/architecture.md)（33–36 行）、[Desktop 实现](../../backend/apps/stravia-desktop/src/lib.rs)（468–545 行）。

## 后端实际执行链

1. HTTP Router 登记 Chat Completions、Responses（含 WebSocket 与 compact）、Anthropic Messages、Embeddings、Gemini 与 MCP 等入口。[Proxy Router](../../backend/crates/stravia-core/src/proxy/server.rs)（24–75 行）、[MCP Server](../../backend/crates/stravia-core/src/mcp/server.rs)（34–66 行）。
2. Ingress codec 将 wire 请求解码为 canonical `AiRequest`，交给 Dispatcher / Inference Run。[Chat ingress](../../backend/crates/stravia-core/src/proxy/ingress/openai_compatible/chat_completions.rs)（16–88 行）、[Dispatcher](../../backend/crates/stravia-core/src/proxy/dispatcher/mod.rs)（16–39 行）。
3. Inference Run 编排授权、Admission、Hook、Model Turn 与最终交付；RouteSelector 选择 Target，Vendor execution lease 锁定连接与插件。[Inference Run](../../backend/crates/stravia-core/src/proxy/dispatcher/inference_run.rs)（14–103 行）、[RouteSelector](../../backend/crates/stravia-core/src/router/selection.rs)（73–128 行）。
4. Vendor 选择出口协议，Host 编码 WIT operation，通过受控网络与 Wasmtime 执行组件，再归一化 canonical 输出。[插件执行](../../backend/crates/stravia-core/src/plugin/execution.rs)（258–313、727–818 行）。
5. 已暴露的 Platform Tool call 在内部执行，结果通过隐藏轮次进入同一 Inference Run 的后续 Model Turn；普通客户端工具仍交客户端处理。[Model Leg](../../backend/crates/stravia-core/src/proxy/dispatcher/inference_run/engine/leg.rs)（500–590、700–804 行）。

不能从“支持协议族”推定任意 Provider、模型和操作均可用；实际能力还取决于已安装 descriptor、模型快照、配置与可表达性边界。[协议 registry](../../backend/crates/stravia-protocol-codec/src/registry.rs)（1–73 行）、[插件协议选择](../../backend/crates/stravia-core/src/plugin/execution.rs)（258–313 行）。

业务 SQL 支持 SQLite/PostgreSQL，PostgreSQL 的 runtime cache 另需 Redis；Artifact 对象支持本地文件或 S3 兼容存储。Redis 不是业务 SQL 后端。[配置](../../backend/crates/stravia-core/src/config.rs)（4–60 行）、[Gateway runtime](../../backend/crates/stravia-core/src/gateway/runtime.rs)（146–210 行）、[Artifact Store](../../backend/crates/stravia-core/src/agent/artifact/store.rs)（1–96 行）。

## 前端现状与边界

前端使用 Svelte 5、SvelteKit 2、TypeScript、Vite、Tailwind CSS 4、shadcn-svelte/Bits UI；TanStack Query/Table 管理数据，Paraglide 提供 `en-US` / `zh-CN`，静态 adapter 输出共用客户端。[依赖](../../frontend/stravia-webui/package.json)（14–84 行）、[i18n 配置](../../frontend/stravia-webui/project.inlang/settings.json)（1–5 行）、[adapter](../../frontend/stravia-webui/svelte.config.js)（1–14 行）。

实际导航覆盖：Console Chat/对话、Providers、Models/Routes、API Keys、接入客户端；媒体理解/生成、Web Search、可逆脱敏、Vendor 插件；日志、统计、额度；设置以及登录/首次设置入口。[AppShell](../../frontend/stravia-webui/src/lib/components/app-shell.svelte)（95–125 行）、[根布局](../../frontend/stravia-webui/src/routes/+layout.svelte)（35–38、157–210 行）、[对话页面](../../frontend/stravia-webui/src/routes/conversations/+page.svelte)（46–50 行）。

管理请求集中经过 `admin-client.ts`：Web 使用同源 Cookie/CSRF；Desktop 通过 Tauri 获取本机端口及 native admin session，再调用 HTTP 管理接口。[客户端](../../frontend/stravia-webui/src/lib/admin-client.ts)（87–145 行）、[认证 transport](../../frontend/stravia-webui/src/lib/auth.ts)（45–67 行）。Console Chat 推理则使用选中的 Stravia API Key 请求 `/v1/responses`，显式不携带管理 Cookie。[Chat adapter](../../frontend/stravia-webui/src/lib/console-chat-adapters.ts)（167–174 行）。

Connect Client 实际配置写入只在 Desktop IPC 中执行；Web 端提供 preview，而非直接改写本地客户端配置。[Apply adapter](../../frontend/stravia-webui/src/lib/connect-client-apply.ts)（11–25 行）。

本轮未启动浏览器或 Desktop，不能证明实际视觉效果、响应式、无障碍或页面操作结果。设计规范自身也声明规范不等于已实现。[DESIGN.md](../../DESIGN.md)（204 行）。

## 值得优先关注的事项

### 1. 已核实：架构文档与源码不一致

- `architecture.md` 仍写五个 Component、四个 dedicated；实际 builder 清单为八个 Component：`base`、`openai-codex`、`xai-grok`、`command-code`、`devin`、`opencode-free`、`claude-code`、`antigravity`。默认仅取 base，`--all` 取全表。[旧架构描述](../design/architecture.md)（702–722 行）、[真实 builder](../../backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs)（10–19、32–38 行）。
- `vendor-plugins.md` 身份映射表漏列 `opencode-free`，虽然后文正确写七个专属插件；实际 guest descriptor 将其标记为 Dedicated。[映射表](../design/vendor-plugins.md)（15–25、85–86 行）、[OpenCode guest](../../backend/crates/stravia-vendor-opencode-free/src/lib.rs)（35–36、321–336 行）。
- 架构文档 WIT 写 `0.2.0`，实际 WIT package 与 Host bindgen 一致为 `0.4.0`。这是文档过时，不是已证实的运行兼容失败。[旧版本](../design/architecture.md)（726 行）、[WIT 声明](../../backend/crates/stravia-vendor-sdk/wit/vendor.wit)（7 行）、[Host binding](../../backend/crates/stravia-vendor-runtime/src/bindings.rs)（1–13 行）。

建议先更新对应架构段落与插件身份表；本轮仅报告，未修订这些文档。

### 2. 历史性能报告记录未达标与诊断缺口

2026-10-09 的四协议真实 HTTP 基准报告记录：250MB 目标未达到，完整 10QPS 矩阵也未通过；两个对比 report 均退出 1。负载为精确 5MB 请求、本地 synthetic upstream，不能外推所有真实业务负载。[历史报告](protocol-http-benchmark.md)（69–117 行）。

同一报告中，SQLite 请求全部成功且 Generation Chain 条数对应请求数，但普通 Observation 投影明显少于请求数，并有大量 `observation finalization unavailable` 告警。PostgreSQL 测量还出现本机 TCP connect timeout；报告明确没有证明真实 Provider 故障，也没有证明缓存导致超时。[有效性与诊断结果](protocol-http-benchmark.md)（119–139 行）。

建议以该基准的同负载、同平台、同成功语义定位热点与记录缺口；不要缩小输入或把失败场景的低资源占用当改善。本轮未重新运行或确认历史失败。

### 3. 信任与数据边界需要准确理解

Vendor 插件获授权处理所选连接的上游凭据；Wasm 不等于插件看不到密钥，也不保证插件不会滥用获准读取的秘密。这是明确设计取舍，不是本轮发现的泄露。[ADR-0068](../adr/0068-trust-vendor-plugins-with-connection-credentials.md)（5–15 行）。

Console Chat 历史保存在 origin 范围 IndexedDB，未加密，不跨 Server/Desktop 同步；清理站点数据、改变 origin 或卸载 WebView 可能导致历史丢失，脚本/XSS 与共享浏览器环境也有读取边界。[Chat 文档](../design/console-chat.md)（31–35 行）、[实际 store 装配](../../frontend/stravia-webui/src/lib/console-chat.svelte.ts)（1–8、43–46 行）。

### 4. 常规测试不等于完整验证矩阵

`task test` 排除 Desktop 与三个 fixture package，真实浏览器、HTTP/存储 E2E、Desktop smoke 另有入口；仅 unit 通过不能宣称全部产品面验证通过。[Taskfile](../../Taskfile.yml)（287–352 行）。

`task test:all` 有 Windows/WebView2 与预装 Docker 镜像前置条件，自动建立隔离 PostgreSQL/Redis，构建、测试与清理合计限制 300 秒。它不是无副作用的只读调查命令。[全量 runner](../../tests/common/run-all.ts)（217–240、500–519 行）。

## 开发与验证入口

以下为仓库定义的命令，不代表本轮执行：

| 目标 | 命令 | 前置条件/范围 |
| --- | --- | --- |
| WebUI / Server 联调 / Desktop 开发 | `task dev:web` / `task dev:server` / `task dev:desktop` | Bun 锁定依赖；后两者准备内嵌 Vendor |
| WebUI / Server / Desktop 构建 | `task build:web` / `task build:server` / `task build:desktop` | 依据目标工具链和平台准备 |
| 静态检查 / 常规单测 | `task check` / `task test` | Vendor/fixture 准备；不等于完整 E2E |
| 真实 Chrome Web Access 回归 | `task test:browser` | Chrome；执行 ignored browser tests |
| Proxy / Admin / SQLite E2E | `task test:e2e:proxy` / `task test:e2e:admin` / `task test:e2e:storage:sqlite` | 隔离实例、本地构建产物 |
| PostgreSQL E2E | `task test:e2e:storage:postgres` | 隔离 `DB_URL`、`STRAVIA_TEST_REDIS_URL`；禁止生产连接 |
| Chromium / Windows Desktop E2E | `task test:e2e:web` / `task test:e2e:desktop` | Chromium 或 Windows/Tauri/WebView2 环境 |
| 完整本地矩阵 | `task test:all` | Windows、Docker、预装隔离服务镜像与构建缓存 |

命令依据：[Taskfile.yml](../../Taskfile.yml)（99–120、263–362 行）。CI 另外覆盖 pinned/stable Rust、Windows Chrome、backend/storage、Chromium 与 Desktop lane；本地 `task test` 不自动等同 CI matrix。[CI 定义](../../.github/workflows/ci.yml)（95–197、199–543 行）。

## 本轮实际运行证据

| 命令 | 实际结果 | 能证明什么 |
| --- | --- | --- |
| `cargo metadata --locked --offline --no-deps --format-version 1` | 退出码 0，输出 workspace JSON | 当前本地 manifest 能由 Cargo 读取；不证明编译或测试通过 |
| `task --list-all` | 退出码 0，列出开发、构建、检查与测试任务 | Taskfile 可加载，任务入口存在；不证明任务执行成功 |
| `rustc --version` | 退出码 0，`rustc 1.98.1 (48a229cea 2026-09-01)` | 本机实际 Rust 版本 |
| `bun --version` | 退出码 0，`1.4.2` | 本机实际 Bun 版本 |
| `./target/debug/stravia-server.exe --help` | 退出码 0，输出 Usage、Server/Advanced/Storage 参数，默认监听 `127.0.0.1:23471` | 现存 Server executable 的 CLI help 能运行 |

`Args::parse()` 在 data-dir prepare/lock、日志、Gateway/HTTP 初始化之前，help 退出不进入这些路径；解析前会调用 `load_dotenv()`，因此不声称完全无配置文件读取。[Server main](../../backend/apps/stravia-server/src/main.rs)（120–145 行）。现存二进制没有在本轮重新构建或验证源码指纹，不能视为当前源码构建通过证明。

未执行：build、lint、formatter、单元测试、HTTP/MCP 行为测试、浏览器或 Desktop 产品验证。未查询当前线上 CI 状态，未审计用户工作树改动，未读取实例 secrets。业务代码、迁移、锁文件和既有设计文件均未修改；仅新增本调查报告。
