# Stravia 仓库开发规范

## 适用范围与事实来源

本规范适用于整个仓库。根文件只保留共享约束和任务入口；仅适用于某个子目录的命令与约定，放在该目录下更近的 `AGENTS.md` 中，不在此重复。

以仓库文件为事实来源。修改行为前，先阅读相关实现与测试；不得在已有约定之外另建一套平行约定。

| 任务场景 | 必读文档与规则 |
|---|---|
| 处理 Issue 或规格 | 对应的本地票据与 `docs/agents/issue-tracker.md`，另见下方「Issue 管理」 |
| 探索领域行为或架构 | `CONTEXT.md`、`docs/adr/` 中相关决策，以及 `docs/agents/domain.md` |
| 设计、实现、修改或评审前端 UI | 完整阅读根目录 `DESIGN.md`，遵循其中的交互与验证要求 |
| 修改已有设计 | `docs/design/` 下的相关文档 |
| 确定工具链或工作流 | `rust-toolchain.toml`、根目录 `package.json`、`uv.lock` 与 `Taskfile.yml`；不要把版本号复制到本文件 |
| 修改数据库 schema 或迁移 | 下方「数据库变更」专项规则 |

## 产品定位与职责归属

Stravia 是本地运行、可自托管的 Agent 基础设施，提供模型接入、平台工具执行、有边界的内置 Agent 循环，以及统一的访问控制、历史、用量统计和可观测性。协议网关是其模型接入层，不是完整产品定位。

Platform Tools 在 Stravia 内部执行，可将结果交给后续模型轮次。内置能力包括 Web Search 与 Media Understanding，通过兼容的模型请求和 MCP 暴露。Agent Definitions 由程序管理并进行版本控制；本产品不是用户自定义 Agent 或可视化工作流搭建器。

| 路径 | 职责 |
|---|---|
| `backend/crates/stravia-core/` | 与传输无关的网关、协议、Provider、平台工具、Agent 执行、能力、存储与管理业务规则 |
| `backend/apps/stravia-server/` | 独立 HTTP 服务与传输适配 |
| `backend/apps/stravia-desktop/` | 基于同一套 Core 行为的 Tauri 壳与桌面集成 |
| `frontend/stravia-webui/` | 服务端与桌面端共用的 Svelte 5 / SvelteKit 管理界面 |
| `backend/crates/stravia-devtools/` | 开发 CLI，包括 `stravia-tools` |
| `tests/e2e/` | 代理、管理与存储的 Python E2E 测试 |

### 架构边界

- `stravia-core` 不得依赖 Tauri、Axum 请求类型或 WebUI 逻辑。传输层负责将请求适配到 Core API。
- 管理业务规则放在 Core 服务中；服务端路由和桌面命令保持薄适配层。WebUI 负责展示与调用管理接口，不得复制后端业务规则。
- 保持协议转换边界明确。Provider 适配器不得承担客户端传输或 UI 行为。
- SQLite 与 PostgreSQL 的行为必须一致，除非已有文档决策明确缩小支持范围。
- 复用现有模块与接口。优先干净切换：迁移受影响的调用方并移除废弃路径，不添加别名、兼容垫片或平行实现。

## 实施约束

- 用最小、连贯的改动修复根因。保留无关工作，不混入无关重构或格式化。
- 遵循受影响子系统的命名、错误处理、模块布局与测试模式。注释解释意图、外部约束或非显然取舍，不复述代码。
- 错误必须显式处理。不得吞掉失败，也不得用休眠、缩短超时、盲目重试或针对特定输入的例外处理掩盖问题。
- `let _ =` 仅用于有意丢弃结果：关闭或析构期间向已关闭通道发送消息、刻意分离的任务句柄，以及不会失败或按不会失败处理的操作（`url.set_*`、向 `String` 执行 `write!`）。产品路径中的文件系统、网络、存储和任务执行失败，必须向上传播或通过 `tracing::warn!` / `tracing::debug!` 记录。测试中的结果丢弃必须对应确实预期且无需处理的失败；不得用 `#[cfg(test)]` 隐藏产品路径中的失败。
- 外部输入一律视为不可信。不得记录、提交或暴露 API keys、tokens、cookies、私钥或生产连接串。
- 复用现有依赖。依赖变化时，使用对应包管理器更新锁文件。
- 不得手工修改生成物，包括 `frontend/stravia-webui/dist/` 和 `frontend/stravia-webui/src/lib/paraglide/`。应修改源文件，再重新生成。
- 未经用户明确要求且未明确影响范围，不得推送、发布、部署、合并或调用生产服务。

### 用户可见文案

- 后端多语言值、UI 回退文案、生成示例及从文档派生的常量，以英文为规范默认值；本地化版本必须显式添加。
- 英文与中文 UI 文案一并审查，确保意图一致、表达清晰。围绕用户目标、动作和可观察结果编写；只有用户需要据此操作或恢复时，才暴露存储、协议或生命周期等内部细节。
- 公共行为或安装配置方式变化时，更新对应的使用、部署或设计文档；仅当变化影响项目定位、核心能力概览、快速开始或用户必须提前了解的限制时，才同步更新 `README.md` 和 `README_CN.md`。

### README 定位与内容边界

- `README.md` 和 `README_CN.md` 是面向用户的项目落地页，不是技术细节说明页、开发记录或修复日志。它们应帮助用户判断「这是什么、适不适合我、如何开始使用」。
- 保留产品价值、主要能力、必要限制、简洁的安装与上手步骤，以及详细文档入口。两种语言的内容和意图保持一致。
- 不得因一次功能实现、Bug 修复或性能优化，向 README 追加协议字段、内部状态机、重试阈值、数据库锁与事务、队列与批处理、存储去重、抓包重组等实现细节。技术契约与原理放入对应的 `docs/` 文档，版本变更放入 `CHANGELOG.md`。
- 详细参数、排障、迁移与运维流程放入对应文档；README 只保留必要提示和链接，不承载完整操作手册。不得为了精简而删去用户决策所需的安全、兼容性或费用风险提示。
- 修改 README 前先判断是否改变用户的选择或首次使用路径；若只是实现方式或缺陷修复，保持 README 不变，不以「公共行为变化」为由机械追加技术说明。

## 命令与验证

除非工作流另有说明，否则从仓库根目录运行命令。以 `Taskfile.yml` 为标准工作流定义。

| 目标 | 命令 |
|---|---|
| 安装锁定版本的 WebUI 依赖 | `task install:web` |
| 开发 WebUI / WebUI 与独立服务端 / 桌面端 | `task dev:web` / `task dev:server` / `task dev:desktop` |
| 构建 WebUI / 服务端 / 桌面端 | `task build:web` / `task build:server` / `task build:desktop` |
| 仓库静态检查 / 支持的单元测试 | `task check` / `task test` |
| 代理 / 管理 E2E | `task test:e2e:proxy` / `task test:e2e:admin` |
| SQLite 存储 E2E | `task test:e2e:storage:sqlite` |
| PostgreSQL 存储 E2E | 设置 `DB_URL`，再运行 `task test:e2e:storage:postgres` |
| 完整后端 E2E 矩阵 | 设置 `DB_URL`，再运行 `task test:e2e` |
| Chromium WebUI E2E | `task test:e2e:web` |
| Windows 桌面冒烟测试 | `task test:e2e:desktop` |

`DB_URL` 必须指向隔离的测试数据库，不得使用生产数据库。

### 按改动范围选择检查

先运行最直接相关的检查，再按风险扩大验证范围：

| 改动范围 | 首轮检查 |
|---|---|
| Rust Core | `cargo test -p stravia-core <test-filter>`，然后 `cargo check -p stravia-core` |
| 独立服务端 | `cargo check -p stravia-server --no-default-features`，以及相关 HTTP E2E 验证 |
| WebUI | `bun run --filter stravia-webui test:unit`，然后 `bun run check:web` 与 `bun run lint:web` |
| 仓库级代码或公共契约 | `task check` 与 `task test`，以及相关产品面的验证 |
| 仅文档 | 核对引用路径、命令与产品表述，不运行无关构建 |

- 修复 Bug 时，先复现失败，再修复根因，并运行能在原始行为下失败的回归检查。
- 测试必须保护可观察行为、边界、不变量、状态转换、优先级或真实错误，不得绑定源码文本或偶然的实现细节。测试必须确定、隔离；E2E 测试不得调用生产服务。
- 验证实际改动的产品面：服务端改动验证 HTTP 行为，WebUI 改动验证浏览器行为，Tauri 改动验证实际桌面应用。编译通过不能证明行为正确。

## Issue 管理

Issue 与规格以本地 Markdown 文件保存在 `.scratch/` 下。创建、读取、评论或解决票据时，遵循 `docs/agents/issue-tracker.md`，包括按功能划分目录、每张票据独立成文件，以及 wayfinding 约定。不得用远程 Issue 系统替代本地工作流。

## 领域文档

本仓库采用单上下文布局：根目录 `CONTEXT.md` 与 `docs/adr/`。阅读和维护领域上下文时，遵循 `docs/agents/domain.md`。使用术语表中的词汇；与相关 ADR 冲突时必须明确指出，不得静默覆盖。领域文档不存在时，遵循专项指南的按需创建策略，不提前搭建文档骨架。

## 数据库变更

数据库 schema 变更包括对以下任一目录中的 SQLx 迁移文件进行修改：

- `backend/crates/stravia-core/migrations/sqlite/`
- `backend/crates/stravia-core/migrations/postgres/`

每次新增或修改 migration，以及其他 schema 变更，都必须：

1. 同步更新所有支持该功能的后端。
2. 使用 `stravia-tools dump-schema` 从全部迁移重新生成 SQLite 与 PostgreSQL 参考 schema，并将生成结果与 migration 一并交付：

   ```bash
   stravia-tools dump-schema --backend sqlite --output docs/database/sqlite.sql
   stravia-tools dump-schema --backend postgres --output docs/database/postgres.sql
   ```

3. 验证相关 SQLite 与 PostgreSQL 存储测试。

`docs/database/postgres.sql` 与 `docs/database/sqlite.sql` 是面向 DBA 的最终结构参考，不包含业务数据或 SQLx 迁移历史，不能用于初始化部署；部署应由服务端应用 migrations。不得手工修改 schema 正文；只有文件头注释允许直接编辑。

PostgreSQL 导出需要指向非生产开发服务器的 `DATABASE_URL`、具有 `CREATEDB` 权限的角色，以及 PATH 中与服务端版本兼容的 `pg_dump`。工具会创建并清理独立临时数据库；不得使用生产连接。工具的目录选择、隔离方式与跨环境比较要求见 `docs/design/architecture.md` 的数据库章节。

## 交付要求

完成前检查 diff，排除无关改动、凭据、占位符或未完成工作。确保受影响的调用方、测试、文档、锁文件和生成参考文件已同步更新，或明确无需修改。

报告改动文件与行为、实际执行的验证命令及结果，以及剩余风险或未能完成的验证。区分既有失败与本次引入的失败；不得将未运行的检查报告为通过。
