# Stravia 仓库开发规范

适用于整个仓库。以仓库文件为事实来源：修改行为前先读相关实现与测试，复用现有约定，不另建平行实现。根文件只放共享约束与任务入口；仅适用于子目录的约定放在更近的 `AGENTS.md`，不在此重复。

## 项目

Stravia 是本地运行、可自托管的 Agent 基础设施（模型接入、平台工具、内置 Agent 循环、统一访问控制与可观测性）。协议网关只是模型接入层；产品不是用户自定义 Agent 或工作流搭建器，定位细节见 `README.md` / `CONTEXT.md`。

| 路径 | 职责 |
|---|---|
| `backend/crates/stravia-core/` | 与传输无关的网关、协议、Provider、平台工具、Agent 执行、存储与管理业务规则 |
| `backend/apps/stravia-server/` | 独立 HTTP 服务与传输适配 |
| `backend/apps/stravia-desktop/` | 基于同一套 Core 行为的 Tauri 壳与桌面集成 |
| `frontend/stravia-webui/` | 服务端与桌面端共用的 Svelte 5 / SvelteKit 管理界面 |
| `backend/crates/stravia-devtools/` | 开发 CLI，包括 `stravia-tools` |
| `tests/e2e/` | 代理、管理与存储的 Python E2E 测试 |

架构边界：

- `stravia-core` 不依赖 Tauri、Axum 请求类型或 WebUI 逻辑；传输层把请求适配到 Core API。
- 管理业务规则在 Core 服务中；服务端路由和桌面命令是薄适配层，WebUI 不复制后端业务规则。Provider 适配器不承担客户端传输或 UI 行为。
- SQLite 与 PostgreSQL 行为一致，除非已有文档决策明确缩小支持范围。
- 优先干净切换：迁移受影响的调用方并移除废弃路径，不加别名、兼容垫片或平行实现。

## 任务入口

| 任务场景 | 必读 |
|---|---|
| Issue 或规格 | `.scratch/` 本地票据与 `docs/agents/issue-tracker.md`；不用远程 Issue 系统 |
| 领域行为或架构 | `CONTEXT.md`、相关的 `docs/adr/`、`docs/agents/domain.md` |
| 前端 UI 设计、实现、评审 | 根目录 `DESIGN.md` 全文 |
| 修改已有设计 | `docs/design/` 下相关文档 |
| 工具链或工作流 | `rust-toolchain.toml`、根 `package.json`、`uv.lock`、`Taskfile.yml`；版本号以它们为准，不复制到本文件 |
| 数据库 schema、migration、升级钩子或启动数据转换 | 修改或评审前必须读取 [migration-startup skill](.agents/skills/migration-startup/SKILL.md)；schema 变更另见下方「数据库变更」 |

## 约定

- 改动聚焦：最小、连贯，修根因；跟随受影响子系统的命名、错误处理、模块布局与测试模式，不混入无关重构或格式化。
- 错误显式处理，不吞失败；不用 sleep、缩短超时、盲目重试或针对特定输入的例外掩盖根因。
- `let _ =` 仅用于有意丢弃：关闭期向已关闭通道发送消息、刻意分离的任务句柄、不会失败的 `url.set_*` / 向 `String` 的 `write!`。产品路径的文件系统、网络、存储、任务执行失败须向上传播或 `tracing::warn!` / `tracing::debug!`；测试中的丢弃须对应确实预期的失败，`#[cfg(test)]` 不隐藏产品路径失败。
- 不手工修改生成物（`frontend/stravia-webui/dist/`、`frontend/stravia-webui/src/lib/paraglide/`）：改源文件后重新生成。
- 依赖变化用对应包管理器更新锁文件。不记录、不提交、不暴露 API keys、tokens、cookies、私钥、生产连接串。未经明确授权与范围确认，不推送、发布、部署、合并或调用生产服务。

### 用户可见文案

- 后端多语言值、UI 回退文案、生成示例及文档派生常量以英文为规范默认值，本地化显式添加；英文与中文 UI 文案一并审查。
- 围绕用户目标、动作和可观察结果编写；只有用户需要据此操作或恢复时才暴露存储、协议、生命周期细节。

### README 边界

`README.md` / `README_CN.md` 是用户落地页，帮助判断「这是什么、适不适合我、如何开始」：仅当变化影响用户选择或首次使用路径时才更新（安全、兼容、费用风险提示不得删）；实现细节进 `docs/`，版本变更进 `CHANGELOG.md`，不因功能实现或缺陷修复追加技术细节。两种语言内容一致。

## 命令与验证

从仓库根目录运行；以 `Taskfile.yml` 为标准工作流定义。

| 目标 | 命令 |
|---|---|
| 准备 Cargo 补丁（直接运行 Cargo 或启动 IDE 前） | `task prepare:crates` |
| 安装锁定版本的 WebUI 依赖 | `task install:web` |
| 开发 WebUI / 服务端 / 桌面端 | `task dev:web` / `task dev:server` / `task dev:desktop` |
| 构建 WebUI / 服务端 / 桌面端 | `task build:web` / `task build:server` / `task build:desktop` |
| 仓库静态检查 / 单元测试 | `task check` / `task test` |
| 完整本地测试矩阵（限时五分钟） | `task test:all` |
| 代理 / 管理 E2E | `task test:e2e:proxy` / `task test:e2e:admin` |
| SQLite 存储 E2E | `task test:e2e:storage:sqlite` |
| PostgreSQL 存储 E2E | 设 `DB_URL` 与 `STRAVIA_TEST_REDIS_URL` 后 `task test:e2e:storage:postgres` |
| 完整后端 E2E 矩阵 | 设 `DB_URL` 与 `STRAVIA_TEST_REDIS_URL` 后 `task test:e2e` |
| Chromium WebUI E2E | `task test:e2e:web` |
| Windows 桌面冒烟测试 | `task test:e2e:desktop` |

`DB_URL` 与 `STRAVIA_TEST_REDIS_URL` 必须指向隔离的测试数据库与 Redis，不得使用生产服务。

`task test:all` 覆盖全部本地测试面，前置条件为锁定依赖、浏览器、Docker 与本地 `postgres:16`、`redis:8-alpine` 镜像；自动创建并清理仅监听 loopback 的独立容器，不使用调用者的数据库或 Redis。五分钟从启动计到测试与清理完成（含检查与增量构建，冷构建不承诺），超时或任一失败返回非零；不得以跳过测试、缩短产品 TTL、丢弃大输入边界或重试掩盖失败来满足时限。需要生产凭据的 live-upstream、S3 等 opt-in 测试不在其内，须单独满足前置条件。日志与计时在 `target/test-results/all/`。

### 单个测试的最快路径

先跑目标用例，通过后再扩大验证；单例调试不必先跑全量。`<…>` 替换为实际目标：

| 测试类型 | 定向命令 |
|---|---|
| Rust 单元测试 | `cargo test --locked -p <crate> --lib <完整测试名> -- --exact` |
| WebUI 单元测试 | `bun test --cwd frontend/stravia-webui <测试文件> -t "<测试名>"` |
| HTTP E2E | `uv run --locked --group test python -m pytest "<文件>::<函数>" -q` |
| Chromium E2E | `bun run --filter stravia-webui test:e2e <测试文件> --grep "<测试名>" --retries=0` |

- E2E 先按 `Taskfile.yml` 对应任务的 `deps` / `env` 准备构建与环境，源码变更后重新校验缓存。WebUI 单元测试生成物未备时先跑 `test:unit`；Rust 集成测试把 `--lib` 换成 `--test <文件名，不含 .rs>`。
- 确认目标用例确实执行并通过；零测试或全部被过滤不算通过。

### 按改动范围选择检查

先跑最直接相关的检查，再按风险扩大：

| 改动范围 | 首轮检查 |
|---|---|
| Rust Core | `cargo test -p stravia-core <test-filter>`，然后 `cargo check -p stravia-core` |
| 独立服务端 | `cargo check -p stravia-server --no-default-features`，加相关 HTTP E2E |
| WebUI | `bun run --filter stravia-webui test:unit`，然后 `bun run check:web` 与 `bun run lint:web` |
| 仓库级代码或公共契约 | `task check` 与 `task test`，加相关产品面验证 |
| 仅文档 | 核对引用路径、命令与产品表述 |

- 修 Bug：先复现失败，再修根因，再跑能在原始行为下失败的回归检查。
- 测试保护可观察行为、边界、不变量、状态转换、优先级或真实错误，不绑定源码文本或实现细节；确定、隔离，E2E 不调用生产服务。
- 验证实际改动的产品面：服务端→HTTP 行为，WebUI→浏览器行为，Tauri→实际桌面应用。编译通过不能证明行为正确。

## 数据库变更

修改 `backend/crates/stravia-core/migrations/sqlite/` 或 `migrations/postgres/` 中的 SQLx 迁移文件即触发，每次新增或修改 migration 都必须：

1. 同步更新所有支持该功能的后端。
2. 用 `stravia-tools dump-schema` 从全部迁移重新生成参考 schema，与 migration 一并交付：

   ```bash
   stravia-tools dump-schema --backend sqlite --output docs/database/sqlite.sql
   stravia-tools dump-schema --backend postgres --output docs/database/postgres.sql
   ```

3. 验证相关 SQLite 与 PostgreSQL 存储测试。

`docs/database/*.sql` 是面向 DBA 的最终结构参考，不含业务数据或迁移历史，不能用于初始化部署（部署由服务端应用 migrations）；正文不手改，仅文件头注释可编辑。PostgreSQL 导出需要指向非生产开发服务器的 `DATABASE_URL`（角色需 `CREATEDB` 权限）与 PATH 中兼容的 `pg_dump`；工具自建并清理独立临时数据库。目录选择、隔离方式与跨环境比较见 `docs/design/architecture.md` 的数据库章节。

## 交付

完成前检查 diff，排除无关改动、凭据、占位符或未完成工作；确保受影响的调用方、测试、文档、锁文件与生成参考文件已同步更新，或明确无需修改。报告改动文件与行为、实际执行的验证命令与结果、剩余风险；区分既有失败与本次引入的失败，未运行的检查不得报告为通过。
