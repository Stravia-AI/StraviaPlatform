# `/settings#proxy` 出站代理链路调查

> **历史证据，已由出站代理切换修复替代。** 下文第 1–7 节冻结记录修复前源码及 2026-10-10 的本机 HTTP 实测；旧键、行号和行为不描述当前实现，既有失败证据不删除、不重写成成功。当前契约见 [架构：共享出站代理配置与独立选择](../design/architecture.md#共享出站代理配置与独立选择)。

## 当前修复与验证边界

- 移除全局启用开关，完整配置以单行 `outbound_proxy` JSON 原子保存/读取。模型及 Web Provider 独立 `use_proxy`，产品更新独立 `update_use_proxy`；设置页不保存更新开关，更新开关不提交代理 URL 草稿。
- 未知 scheme 在保存和使用边界拒绝，URL 为空只代表未配置；需要代理的消费者不得静默直连。支持 HTTP(S)、SOCKS5、SOCKS5h。Core reqwest 的 SOCKS5 为本机 DNS、SOCKS5h 为代理端目标域名解析；LocalWeb 保留既有 socks5 规范化为 socks5h 的代理侧 DNS 策略，本次没有新增浏览器本机 DNS 行为。
- bypass 接入模型 HTTP/WS、Web Access HTTP/浏览器及更新请求/下载的目标路由，客户端与 WS 复用身份包含 bypass。Direct 不继承环境代理；匹配绕过规则仍须通过既有目标授权/SSRF 检查。
- SQLite/PostgreSQL 迁移 0016 保留旧有效模型路由及 Web 独立选择、初始化更新偏好、合并配置并删除旧键。管理 API 拒绝重新创建旧代理键。
- Desktop updater 通过 Tauri `configure_client` 注入同一显式代理/no-proxy filter，而不是修改进程 `NO_PROXY`。锁定 `tauri-plugin-updater` 2.12.0 的源码调用点：`src/updater.rs` 的 352–362（保存回调）、497–525（检查）、588–606（传递给 Update）、691–715（下载）。这是依赖源码依据，不是本轮下载验证成功的声明。

新增永久回归覆盖管理 HTTP 的代理命中/绕过、未知 URL 拒绝且原配置不变、SQLite 注入真实写失败保留整行、Provider/更新选择独立，以及真实 SQLite/PostgreSQL 数据迁移；Core 回归覆盖 SOCKS5h 域名握手与 WS bypass 身份切换。以下是集成验证的实际结果；第 6 节的 PASS 仍只属于修复前历史 smoke。

### 集成验证（2026-10-10）

| 验证面 | 实际命令或操作 | 结果 |
| --- | --- | --- |
| WebUI 单测 | `bun run --filter stravia-webui test:unit` | 329 passed，0 failed。 |
| WebUI 静态检查 | `bun run check:web`、`bun run lint:web`、`bun run format:check:web`；更新组件 `svelte-autofixer` | 类型检查 0 errors / 0 warnings；lint、格式检查退出 0；autofixer 无 issues / suggestions。 |
| 完整设置页 Chromium 回归 | `PLAYWRIGHT_USE_PREBUILT=1`，`bun run --filter stravia-webui test:e2e settings.spec.ts --workers=1 --retries=0` | 29 passed，48.7 秒。 |
| Core 全量 lib 单测 | `cargo test --locked --jobs 4 -p stravia-core -p stravia-web-access -p stravia-desktop --lib` | Core 1164 passed、1 failed、4 ignored；失败为下述 compaction 超时，Cargo 随后停止该组合命令。 |
| Desktop lib 单测 | `cargo test --locked --jobs 4 -p stravia-desktop -p stravia-web-access --lib --no-fail-fast` 的 Desktop 部分 | 44 passed；包括真实本机 HTTP redirect / bypass 和更新独立选择回归。 |
| Local Web lib 单测 | 修正新增 bypass 反例及移除内部 timeout 错误类型断言后，`cargo test --locked --jobs 4 -p stravia-web-access --lib` | 133 passed，7 ignored；未缩短产品超时、增加重试或放宽 SSRF 检查。 |
| 受影响 Rust targets | `cargo clippy --locked --jobs 4 -p stravia-core -p stravia-web-access -p stravia-server -p stravia-desktop --all-targets -- -D warnings` | 退出 0。受影响文件按各 crate 的实际 edition 单独执行 rustfmt check，退出 0。 |
| 管理 HTTP 代理回归 | `STRAVIA_BINARY=target/test-artifacts/services/stravia-server.exe`，`uv run --locked --group test python -m pytest tests/e2e/admin/test_admin.py -k proxy -q --tb=short` | 7 passed，39 deselected；真实 SQLite INSERT/UPDATE trigger 拒绝写入后，整行旧配置不变。测试显式关闭外部 SQLite connection，避免 Windows teardown 文件锁。 |
| 双后端迁移与完整存储矩阵 | 先执行 `tests/e2e/storage/test_outbound_proxy_migration.py`，再执行 `uv run --locked --group test python -m pytest tests/e2e/storage -q --tb=short` | 迁移 2 passed；完整 SQLite/PostgreSQL 存储矩阵 32 passed，126.15 秒。使用独立 loopback PostgreSQL/Redis 容器，不连接用户数据库。 |
| 双后端结构参考 | `stravia-tools dump-schema --backend sqlite --output docs/database/sqlite.sql`；同命令 PostgreSQL 后端输出 `docs/database/postgres.sql` | 两次退出 0；PostgreSQL 临时导出数据库已清理。 |
| Windows Native 冒烟 | `task --color=false test:e2e:desktop` | 11 passed（43.2 秒），最终迁移后重新构建的专用 Tauri/WebView 产物与原生认证 HTTP；驱动记录一次 execute timeout 警告，但用例和进程退出 0。 |

Core 的新增/受影响代理回归均在上述全量运行中通过，包括 unsupported URL 安全拒绝、SOCKS5h 远程域名 HTTP 传输、环境代理隔离、更新独立选择与重定向 bypass、remote Web bypass、Local Web run 快照，以及 `reusable_websocket_does_not_cross_effective_proxy_changes`。

独立产品 smoke 使用本轮构建并保存的 `services` Server，真实调用 `/v1/chat/completions` 与模型发现接口。本机 origin/proxy 计数确认同一进程内「代理 → bypass 直连 → 再次代理」、发现绕过、Provider 关闭直连、更新偏好不改变模型出口，以及非法 scheme 拒绝且完整旧配置保留。SOCKS5h fixture 收到地址类型 3 和 `unresolved-proxyfix.invalid`；SOCKS5 fixture 收到 IPv4 地址类型 1。另一个临时 SOCKS5 fixture 以 `localhost:9` 为目标，实际收到地址类型 4 的 `::1`，确认本机 DNS 后传 IP，而非把 `localhost` 交给代理解析。10 个脚本场景及该额外 DNS 场景均通过；fixture 关闭退出 0。

实际 Chromium 页面通过本机登录和真实管理接口验证：没有全局启用开关；失败保留 URL/bypass 草稿与旧快照；成功只发送一次完整 PUT；隐藏的 `force_http1=true` 不丢失；更新开关不提交另一张表单的草稿。对更新偏好写入注入 HTTP 200 的应用错误后，开关和服务端已确认值均保持 true；解除注入后能保存 false。英文和中文均观察实际页面，390px 窄屏无 document 横向溢出。公网更新检查在页面验证中被本地拦截，不作为本轮更新检查成功证据。

管理接口沿用既有 **HTTP 200 + `error` 应用错误 envelope**；因此拒绝 scheme、拒绝缺少 URL 的更新启用、存储写失败，应检查 `error`、没有 `ok` 和持久化快照，而不是擅自将整个管理协议改成 HTTP 400。

为隔离 Native 冒烟，`desktop-e2e` feature 关闭启动时的远程 Catalog origin 与后台刷新；生产 Desktop 的 Catalog 行为不变。通过 computer 启动独立临时数据目录并取得原生窗口截图；前台控制申请超时后已释放，没有操作已有实例，后续 Native UI 验证使用仓库既有隔离 WDIO 流程。

最后一轮迁移复验补齐了「已有 Provider，但四个旧代理键和新配置都不存在」：旧全局缺省 false，因此旧 true 必须迁为 false，而不是保留 true 后让空 URL 阻断模型调用。SQLite 的实际 SQL 在修复前断言失败，修复后通过；两后端都新增 `missing_all_legacy`、`new_only`、`saved_config_disabled` 场景，并把 `fresh` 改为在迁移后创建模型 Provider。现有新配置且没有旧键时保持新选择；有旧键时仍保持旧有效门限的优先级。

最终 services 产物重新构建退出 0 后，双后端迁移回归 **2 passed（2.58 秒）**，完整存储矩阵再次 **32 passed（118.92 秒）**，管理 HTTP 代理回归再次 **7 passed（4.33 秒）**，受影响四个 Rust package 的 all-targets clippy 和新 harness rustfmt check 再次退出 0。双后端结构参考再次按当前 migration 生成；PostgreSQL 临时导出数据库剩余 0，结构参考无内容差异。

另一次真实 Server 升级 smoke 在隔离数据目录中撤销仅数据迁移 0016 的历史记录及六个新旧代理键，保留旧 Provider 的 `use_proxy=true`，随后重启并让正式 SQLx 路径升级。实际 API 读到 false；`GET /api/v1/providers/{id}/test-models` 返回 `['probe-model']`，本机 origin 精确收到 1 次直连请求，未要求空代理 URL。初次临时脚本误用了不存在的 discovery 路由，修正为现有接口后通过；没有改动产品路由。

### 已知限制与未通过的全仓检查

- Core 全量并行运行的 `proxy::server::compaction_tests::registry_failure_gates_http_native_publication_and_standalone_compaction` 触发原有 20 秒测试截止时间。使用同一已编译测试 executable 单独执行该用例，1 passed，1.38 秒。未调整截止时间或加重试；全量运行仍记录为失败，不能据单例通过声明全仓通过。并发资源争用仅为推断，根因未证实。
- 全工作区 clippy 被未修改的 `stravia-vendor-sdk/fixtures/lifecycle-contract/src/lib.rs:399` 的 `clone_on_copy` 阻止；受影响四个产品 package 的全部 targets clippy 已通过。
- 全工作区 rustfmt check 还报告未修改的既有品牌/图标文件：`stravia-core/src/gateway/icon.rs`、`src/plugin/manager.rs`、`tests/admin.rs`。未做范围外格式化；本次所有受影响 Rust 文件格式检查通过。
- 未验证真实签名更新包的下载/安装、HTTPS CONNECT/代理认证和 HTTP/2 协商。Desktop 的本机 HTTP 回归验证生产配置回调的实际路由；Tauri 上游 manifest/download 都调用该回调的结论来自锁定依赖源码，不能扩写为真实更新安装成功。Native E2E 下载/安装桥也不是生产更新证据。

本轮创建的 PostgreSQL/Redis 容器、临时升级/HTTP/Native 数据、TCP fixtures 和 schema bridge/临时脚本已清理。保留仓库标准工作流的编译缓存与验证证据；未停止 Docker Desktop 或已有用户应用，未提交、推送或执行生产更新。

---

| 项目 | 范围与结论 |
| --- | --- |
| 研究截点 | 2026-10-10，本仓库当前源码与本地依赖源码。 |
| 调查边界 | 不修改产品，不新增测试，不调用真实模型上游或用户实例；HTTP 行为验证只访问本机，构建前置依赖准备可能访问包仓库。下文既有测试仅为源码断言，不代表本次执行通过。 |
| 核心结论 | **这不是所有出站流量的总开关。供应商请求由 Provider `use_proxy` 与全局 `proxy_enabled` 联合决定；Web Access 不读取该全局开关；`proxy_bypass` 有 UI 保存路径，但未发现后端消费者。** |
| 生效时机 | 供应商执行开始时读取当前全局设置并捕获 HTTP/WS 客户端，无须重启才能让后续执行使用新设置；已经发出的请求不会被重新路由。 |

## 1. 截图字段、保存与持久化

设置页分别查询 `proxy_enabled`、`proxy_url`、`proxy_bypass`。URL 的 `http://127.0.0.1:7890`、bypass 的 `localhost,127.0.0.1,.internal` 是 **placeholder 示例，不是默认设置值**。源码不能反推截图时数据库的实际值。开关改变只更新 `proxyDraft`，点击保存才发请求；开启状态唯一字段校验是 trim 后 URL 非空，没有协议、端口或 bypass 语法校验。[设置 query/草稿：`+page.svelte:121–165`](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L121-L165)，[保存：206–248](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L206-L248)，[控件：584–614](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L584-L614)。

保存链路：

```text
settings 页面本地草稿
  → Promise.all(三个 saveSetting)
  → PUT /api/v1/settings/{proxy_enabled|proxy_url|proxy_bypass}
     body: {"value":"字符串"}
  → admin settings handler
  → AdminService::set_setting
  → SettingsStore::set(key, value)
  → SQLite / Postgres settings(name, value, updated_at)
```

页面把 enabled 写为 `true`/`false`，URL/bypass trim 后写入。客户端 GET 解包 `{"data": string|null}`；PUT 成功返回 `{"ok":true}`。普通代理键在 Core 通用设置服务中没有 URL/布尔白名单校验。没有存储值时是 `None`/`null`，不是注入占位示例。[客户端 `admin-client.ts:114–128,393–396`](../../frontend/stravia-webui/src/lib/admin-client.ts#L114-L128)，[settings handler:5–28](../../backend/apps/stravia-server/src/admin_routes/settings.rs#L5-L28)，[Core settings:6–90](../../backend/crates/stravia-core/src/admin/settings.rs#L6-L90)，[SettingsStore:427–429](../../backend/crates/stravia-core/src/storage/traits.rs#L427-L429)，[SQLite 实现:8–26](../../backend/crates/stravia-core/src/storage/sqlite/settings.rs#L8-L26)，[Postgres 实现:8–26](../../backend/crates/stravia-core/src/storage/postgres/settings.rs#L8-L26)。

**保存不是原子提交。** 三个独立 PUT 没有事务或失败回滚；`Promise.all` 中某一项失败时其他项可能已写入。每个成功的 `saveSetting` 独立 invalidate query，只有全部成功才统一写 cache、清草稿。失败保留草稿并 toast 错误。保存期间开关和按钮 disabled，但 URL/bypass 输入没有对应 disabled。部分持久化以及期间继续编辑的具体时序风险均为源码推导，未做失败注入复现。[`saveSetting` / `saveProxy`:206–248](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L206-L248)，[输入与按钮:592–614](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L592-L614)。

API 在 admin router 权限层下：Server 使用 access cookie 并对非 GET 做 origin/CSRF 检查，Desktop 使用 Bearer 管理会话；并非匿名代理配置接口。[router:311–317](../../backend/apps/stravia-server/src/admin_routes/mod.rs#L311-L317)，[HTTP auth:61–92](../../backend/apps/stravia-server/src/http_auth.rs#L61-L92)。

## 2. 供应商联合决策与请求链路

已保存 Provider 的 `use_proxy` 来自连接快照；候选配置验证/OAuth session 则从 operation metadata 中取得 bool，缺省 false。全局 bool 解析接受 trim、转小写后的 `1|true|yes|on`，其他值为 false。[`provider_snapshot`:856–880](../../backend/crates/stravia-core/src/plugin/execution.rs#L856-L880)，[session execution:613–646](../../backend/crates/stravia-core/src/plugin/execution.rs#L613-L646)，[`parse_bool_setting`:1010–1016](../../backend/crates/stravia-core/src/gateway/runtime.rs#L1010-L1016)。

| Provider/session `use_proxy` | 全局 `proxy_enabled` | URL | 供应商 HTTP 与 WS |
| --- | --- | --- | --- |
| false / 未提供 | 任意 | 任意 | Direct；不读代理设置，不继承系统/环境代理。 |
| true | false / 缺失 / 不识别的字符串 | 任意 | Direct；不要求 URL 有效。 |
| true | true | 缺失 / 空 / 仅空白 | `proxy_url is empty`，执行准备失败。 |
| true | true | 非空 | trim 后交给 `reqwest::Proxy::all`；解析/建 client 失败向上返回，不静默回退直连。 |

证据为 [`effective_vendor_proxy`:905–942](../../backend/crates/stravia-core/src/gateway/runtime.rs#L905-L942) 与 [`client_for_vendor`:944–982](../../backend/crates/stravia-core/src/gateway/runtime.rs#L944-L982)。

```text
Provider/候选连接快照的 use_proxy
  → execute_vendor_input
  → Gateway::vendor_client_snapshot(use_proxy)
  → effective_vendor_proxy（读取全局 enabled / URL / force_http1）
  → client_for_vendor(proxy, false) + client_for_vendor(proxy, true)
  → VendorNetwork（HTTP client、WS client、授权 origins、operation）
  → 插件 Host HTTP / WS 网络请求
```

`execute_vendor_input` 在创建 VendorNetwork 前获取客户端；`?` 将 settings/代理构建错误返回调用方，没有失败后改用直连的分支。HTTP 与 WS 捕获同一个有效代理决策。[execution:729–785](../../backend/crates/stravia-core/src/plugin/execution.rs#L729-L785)，[snapshot:892–903](../../backend/crates/stravia-core/src/gateway/runtime.rs#L892-L903)。传输层随后仍校验授权 origin 与重定向，不把代理选择当成目标访问授权。[VendorNetwork:311–462](../../backend/crates/stravia-core/src/plugin/network.rs#L311-L462)。不同产品调用有各自的错误包装，不能把某一个管理接口错误码视为所有供应商请求的统一响应。

## 3. HTTP/1、运行时快照、缓存与 WebSocket 复用

### 3.1 HTTP/1 与客户端缓存

Gateway 启动构造两个 Direct client，均 `.no_proxy()` 和禁自动 redirect；WS client 额外 `.http1_only()`。显式代理 client 同样禁自动 redirect；HTTP 是否仅 HTTP/1 由 `proxy_force_http1` 决定，WS 始终仅 HTTP/1。`proxy_force_http1` 被 runtime 读取，但本设置页只保存三个键，不提供这个键的开关。[启动构造:267–282](../../backend/crates/stravia-core/src/gateway/runtime.rs#L267-L282)，[全局读取与构造:932–982](../../backend/crates/stravia-core/src/gateway/runtime.rs#L932-L982)。

| client | Direct | Explicit，force=false | Explicit，force=true |
| --- | --- | --- | --- |
| HTTP | 启动时 Direct HTTP client | 一般 HTTP client；不是强制 HTTP/1，也不保证实际协商 HTTP/2 | HTTP/1 only |
| WS | 启动时 Direct WS client，HTTP/1 only | HTTP/1 only | HTTP/1 only |

显式缓存是两个槽位 `[None, None]`，而非无限增长的 URL map：槽位由最终 `require_http1 || force_http1` 决定，键为 `"{proxy_url}|{force_http1}"`。同槽、同键返回 reqwest client clone，复用连接池；不同键在构造成功后替换槽位。Direct 不写此缓存，也不清掉旧显式槽位。强制 HTTP/1 时 HTTP/WS 可命中同一槽位。URL 只 trim，没有应用级语义规范化：等价但文本不同的 URL 可产生不同 cache key。[初始化:500–517](../../backend/crates/stravia-core/src/gateway/runtime.rs#L500-L517)，[`client_for_vendor`:944–982](../../backend/crates/stravia-core/src/gateway/runtime.rs#L944-L982)。

### 3.2 生效时机与非原子读取

Provider `use_proxy` 在准备执行时捕获；全局 enabled、URL、force 三项在实际 `vendor_client_snapshot` 调用时逐次异步读取。之后该执行持有 client clone，不会因数据库值变化而切换在途请求。因此后续执行能应用保存后的配置，不要求 Gateway 重启；这不意味着正在运行的流会立即换代理，也不意味着保存会主动关闭旧 socket。[Provider prepare:494–518](../../backend/crates/stravia-core/src/plugin/execution.rs#L494-L518)，[execution:752–785](../../backend/crates/stravia-core/src/plugin/execution.rs#L752-L785)，[runtime:892–982](../../backend/crates/stravia-core/src/gateway/runtime.rs#L892-L982)。

由于设置页逐键提交且 runtime 逐键读取，**保存期间的执行可能看到过渡组合**（例如 enabled 已变而 URL 尚未变）。这是源码支持的潜在时序风险，未在本次复现；不能将“执行客户端快照”误称为数据库三个设置键的事务快照。

### 3.3 WS 复用身份隔离

`EffectiveVendorProxy::reuse_identity` 序列化并 hash：

- Direct：`("direct", use_proxy)`；所以 Direct(false) 与 Direct(true) 虽都直连，但身份不同。
- Explicit：`("explicit", trimmed_proxy_url, force_http1)`；URL 或 force 改变会改变身份。

`websocket_scope_key` 再纳入 Provider ID（或 session 标识）、插件 identity、凭据 hash、代理 identity，传入共享 WS pool。故后续执行不会按新代理身份复用旧身份的闲置连接。`force_http1` 改变也会换 WS scope，即使 WS 本来一直是 HTTP/1。这里是**复用隔离**，不是立即杀掉所有旧连接；旧配置以后重新出现时身份也会再次相同。[identity:984–1009](../../backend/crates/stravia-core/src/gateway/runtime.rs#L984-L1009)，[`websocket_scope_key`:1098–1123](../../backend/crates/stravia-core/src/plugin/execution.rs#L1098-L1123)，[pool 接入:760–785](../../backend/crates/stravia-core/src/plugin/execution.rs#L760-L785)。

## 4. 其他出站消费者：开关覆盖并不一致

| 消费者 | 代理决策 | 生效边界 / 证据 |
| --- | --- | --- |
| Provider 推理、模型发现、额度查询、媒体供应商操作 | Provider `use_proxy` + 全局 enabled/URL | 共享 vendor execution；[发现:74–138](../../backend/crates/stravia-core/src/admin/routes/model_discovery.rs#L74-L138)，[额度:519–623](../../backend/crates/stravia-core/src/admin/provider_allowance/service.rs#L519-L623)，[媒体:213–217](../../backend/crates/stravia-core/src/media_generation/execution.rs#L213-L217)。 |
| OAuth 授权/token 步骤、刷新，候选配置验证 | 候选/session 或已存 Provider `use_proxy` + 全局 enabled/URL | [OAuth:226–304](../../backend/crates/stravia-core/src/admin/oauth.rs#L226-L304)，[验证:226–231](../../backend/crates/stravia-core/src/admin/provider_connection.rs#L226-L231)，[session 执行:583–648](../../backend/crates/stravia-core/src/plugin/execution.rs#L583-L648)。浏览器回调 listener 是入站/本机流量，不能与 token 出站交换混为一谈。 |
| Web Access remote Exa/Zhipu 与 Local Search/Fetch/浏览器渲染 | **Web Provider `use_proxy` + 共享 `proxy_url`；不读全局 enabled** | 在 capture_run_snapshot 时构建并保存 adapters；该 run 不随之后的设置变更。全局关闭但 Web Provider 启用代理时仍可代理。[service:94–151,260–298,340–480](../../backend/crates/stravia-core/src/web_access/service.rs#L340-L480)。 |
| Gateway GitHub release/manifest 检查 | 全局 enabled/URL；关闭明确 `.no_proxy()` | 每次 client 构造读设置。开启且 URL 空/解析失败为 `UPDATE_PROXY_INVALID`，读取失败为 `UPDATE_SETTINGS_UNAVAILABLE`。[updates:228–281](../../backend/crates/stravia-core/src/admin/updates.rs#L228-L281)。 |
| Desktop updater 检查/下载 | 全局 enabled/URL；关闭 `.no_proxy()` | 使用 Tauri updater，而非上述 reqwest release client；URL parse 后交给 updater。[product_update:187–258](../../backend/apps/stravia-desktop/src/product_update.rs#L187-L258)。 |
| Provider Catalog host HTTP 拉取 | 不读 UI 代理键；`.no_proxy()` | 独立 client，[source:1–62](../../backend/crates/stravia-core/src/provider_catalog/source.rs#L1-L62)。 |
| Catalog guest sync | 不读 UI 代理键；使用启动时 Direct clients | [runtime:500–512](../../backend/crates/stravia-core/src/gateway/runtime.rs#L500-L512)，[catalog_sync:40–66,168–237](../../backend/crates/stravia-core/src/plugin/catalog_sync.rs#L40-L66)。 |
| S3 artifact 操作 | 不读 UI 代理键；专用 `.no_proxy()` client | [s3:64–106](../../backend/crates/stravia-core/src/agent/artifact/s3.rs#L64-L106)。 |
| Local Web 的 System 库模式 | 自己读环境代理及 `NO_PROXY`，不是 UI 设置 | Core Web Access service 当前选 Direct/Explicit，不选 System；[outbound:87–147,189–268](../../backend/crates/stravia-web-access/src/outbound.rs#L87-L147)。Chrome CDP 本机控制 client 另 `.no_proxy()`：[cdp:50–70](../../backend/crates/stravia-web-access/src/browser/cdp.rs#L50-L70)。 |

## 5. `proxy_bypass`、环境代理与 URL 处理边界

本次全仓搜索中，`proxy_bypass` 命中设置页读写与 E2E 初始化值，没有后端消费者命中。因此它能作为普通字符串持久化，但没有接入后端绕过规则。**本轮隔离实测：保存 `localhost,127.0.0.1,.internal` 后，对 `127.0.0.1` 上游的推理和模型发现仍命中显式代理，不命中 origin；见第 6 节。**[设置页:129–132,224–242](../../frontend/stravia-webui/src/routes/settings/+page.svelte#L224-L242)。

Direct 供应商客户端显式 `.no_proxy()`；即使进程环境有 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`，Direct 路径也不是“交给环境决定”。Explicit 使用 `Proxy::all(proxy_url)`，不是按目的域名读取 UI bypass。Local Web 自身 System 模式的 `NO_PROXY` 规则是另一套库能力，不应等同于该 UI 字段。[Gateway:267–282,944–982](../../backend/crates/stravia-core/src/gateway/runtime.rs#L944-L982)，[Local Web outbound:189–268](../../backend/crates/stravia-web-access/src/outbound.rs#L189-L268)。

### 5.1 reqwest 0.13.5 的协议、认证与错误边界

锁定版本为 [Cargo.lock:6428–6469](../../Cargo.lock#L6428-L6469)。本地证据目录为 `C:/Users/Chikage/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/reqwest-0.13.5/`，`.cargo_vcs_info.json` 记录 commit `de55373434f07f42926599dbb5a88550d8e55112`；匹配器依赖为本地 `hyper-util-0.1.21/src/client/proxy/matcher.rs`。下列上游链接用于标识对应源码，调查没有访问外网。

- `Proxy::all` 的 “all” 指 HTTP 和 HTTPS **目标**，不是任意目标协议。代理地址本身的 `http://`/`https://` 被 Hyper-util 识别；无 scheme 输入会尝试补 `http://`。`IntoUrl` 首先要求 URL 有 host，并非只接受 HTTP(S) 的协议白名单。[reqwest `proxy.rs:120–160,214–232`](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/proxy.rs#L120-L160)，[`into_url.rs:22–38`](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/into_url.rs#L22-L38)，[Hyper-util matcher:323–365](https://github.com/hyperium/hyper-util/blob/v0.1.21/src/client/proxy/matcher.rs#L323-L365)。
- **非空且可解析为 URL，不等于代理方案可用。** Hyper-util 对不支持的代理 scheme 返回 `None`；matcher `build()` 不是 `Result`。例如具有 host 但 scheme 不被识别的 URL 会让显式规则为空，随后请求不被代理拦截而走直连。这与应用在已返回解析/构建错误时没有直连 fallback 是两回事：库层可能根本没有产生错误。**本轮隔离 HTTP smoke 已确认 `invalid://127.0.0.1:7890` 与 `ftp://127.0.0.1:7890` 保存成功后，模型发现直连本机 origin；见第 6 节。**[matcher `Builder::build`:303–320、`parse_env_uri`:333–353](https://github.com/hyperium/hyper-util/blob/v0.1.21/src/client/proxy/matcher.rs#L303-L353)，[reqwest `into_matcher`:366–416](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/proxy.rs#L366-L416)。
- 工作区声明 `default-features=false`，开启 `rustls-no-provider/system-proxy`；Core 追加 charset/form/http2/json/rustls/stream，没有显式声明 reqwest `socks`。Hyper-util 识别 SOCKS scheme 不等于最终应用具有 SOCKS transport。本轮还运行 `cargo tree --locked -p stravia-server -e features -i reqwest`，确认独立 Server 的 resolved reqwest 0.13.5 feature graph 中没有 `socks`；不应将供应商 reqwest 路径视为支持 SOCKS。未实测 SOCKS 请求，也未核实 Desktop 的独立依赖图。Local Web 的 wreq/System 模式不能用于推断供应商 reqwest 的能力。[workspace manifest:38–40](../../Cargo.toml#L38-L40)，[Core manifest:26–29](../../backend/crates/stravia-core/Cargo.toml#L26-L29)。
- URL 中 HTTP(S) proxy userinfo 会 percent-decode 并生成 Basic `Proxy-Authorization`；用户名/密码中的 URL 保留字符需百分号编码。HTTP 请求走 forward；HTTPS 目标使用 CONNECT，并向 tunnel 传代理认证。代理认证与供应商 API 认证是不同层。[matcher:355–365](https://github.com/hyperium/hyper-util/blob/v0.1.21/src/client/proxy/matcher.rs#L355-L365)，[reqwest connect:795–856](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/connect.rs#L795-L856)。本次未验证代理认证或 HTTPS CONNECT。
- `.proxy(...)` 会关闭自动 system proxy；`.no_proxy()` 清空显式列表并关闭自动 system proxy。`Proxy::all` 自身默认没有 no-proxy filter，`into_matcher` 将缺省 filter 转为空串，因此**环境 `NO_PROXY/no_proxy` 不会自动作用于供应商的显式 `Proxy::all`**。代码也未调用 `Proxy::no_proxy` 传入 UI bypass。[ClientBuilder:1422–1439](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/async_impl/client.rs#L1422-L1439)，[Proxy matcher:366–385](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/proxy.rs#L366-L385)。
- `Proxy::all(...)`/client build 的实际错误会传播；DNS、代理可达性、拒绝、TLS 等通常在请求 `send()` 阶段才体现，设置 PUT 成功不是连接检查成功。reqwest build 还可能因 TLS backend 或 resolver 初始化失败而失败。[ClientBuilder::build:408–420](https://github.com/seanmonstar/reqwest/blob/v0.13.5/src/async_impl/client.rs#L408-L420)。

## 6. 已有测试断言与本轮隔离 HTTP 实测

以下仅记录既有测试的设计和断言，本调查没有执行它们：

| 测试符号 | 源码断言 |
| --- | --- |
| `direct_and_explicit_vendor_clients_ignore_ambient_proxy` | 在子进程固定 HTTP(S)/ALL_PROXY、清空 NO_PROXY；Direct 请求命中 origin，Explicit 命中显式 proxy，不被 ambient proxy 接管。覆盖两种 HTTP/1 标志。[runtime:1037–1140](../../backend/crates/stravia-core/src/gateway/runtime.rs#L1037-L1140)。 |
| `proxied_http_requests_reuse_connections_across_vendor_snapshots` | 两次 snapshot 请求命中同一个代理 peer，断言连接集合大小为 1，说明测试目标包含 client/连接池复用。[runtime:1250–1321](../../backend/crates/stravia-core/src/gateway/runtime.rs#L1250-L1321)。 |
| `reusable_websocket_does_not_cross_effective_proxy_changes` | 同会话先发两次 Direct 并共用 1 条 WS；启用全局代理后第三次遇拒绝代理失败，proxy 接到连接，旧上游请求数不增加。[websocket:149–245](../../backend/crates/stravia-core/src/proxy/dispatcher/inference_run/tests/websocket.rs#L149-L245)。 |

本轮执行了独立 HTTP smoke，而不是 pytest suite。复用 [`admin/conftest.py`](../../tests/e2e/admin/conftest.py#L20-L85) 的初始化/登录/临时 SQLite 生命周期、[`test_admin.py`](../../tests/e2e/admin/test_admin.py#L29-L130) 的本机可计数 probe endpoint 和 Provider 创建 helper；通过管理 API 创建模型、API Key 并调用真实 Server `/v1/chat/completions` 与 `/providers/{id}/test-models`。Origin 与代理替身都只监听 `127.0.0.1` 动态端口；代理替身返回确定的本地上游协议响应，不转发到 origin，因此两端命中计数互斥。没有联系真实模型服务、更新服务或生产实例。

实际命令与结果：

- `bun tests/common/build-test-artifacts.ts --status services`：返回 1，旧产物缓存不适用于当前源码；未用旧二进制作验证。
- `task --color=false build:test:services`：完成 WebUI、vendor 前置准备及 Server/devtools release 构建；Rust 日志 `Finished release profile`。使用新生成的 `target/test-artifacts/services/stravia-server.exe`。
- `uv run --locked --group test python -m target.proxy-investigation-smoke`：退出 0，打印 `result: PASS`，8 个场景。临时脚本只调用 helper，不执行现有测试；完成后删除脚本、清理 server/mock 与临时数据库。

下表计数为每次调用前后的增量；除首两行外 Provider 与全局开关均开启。合法代理为本机 `http://127.0.0.1:<动态端口>`。

| 实测场景 | HTTP 状态 / API 内容 | Origin 命中 | Proxy 命中 |
| --- | --- | --- | --- |
| 推理：全局关闭，Provider 开启 | 200，确定的助手回答 | 1 | 0 |
| 推理：全局开启，Provider 关闭 | 200，确定的助手回答 | 1 | 0 |
| 推理：双方开启，bypass 为空 | 200，确定的助手回答 | 0 | 1 |
| 推理：bypass=`localhost,127.0.0.1,.internal` | 200，确定的助手回答 | 0 | 1 |
| 模型发现：保留同一 bypass | 200，`data=["probe-model"]` | 0 | 1 |
| 模型发现：代理 URL 为空 | PUT 保存 200；发现 HTTP 200，但 body 为 `error`，包含 `proxy_url is empty`，无 data | 0 | 0 |
| 模型发现：`invalid://127.0.0.1:7890` | PUT 保存 200；发现 200，`data=["probe-model"]`，实际直连 | 1 | 0 |
| 模型发现：`ftp://127.0.0.1:7890` | PUT 保存 200；发现 200，`data=["probe-model"]`，实际直连 | 1 | 0 |

所有配置切换发生在同一 Server 进程内，证实后续 Vendor HTTP 操作无须重启即可改变出口。空 URL 的发现接口仍返回 HTTP 200，必须看响应中的 `error`，不能只看 HTTP 状态判断成功。

未验证：浏览器表单交互、三键部分保存/并发竞态、PostgreSQL 持久化、真实 SOCKS/HTTPS CONNECT/代理认证、HTTP/2 协商、WS 实际复用、Web Access Chromium、更新下载、环境变量隔离。相关章节仅陈述源码依据，不将本机 HTTP smoke 外推为这些产品面的验收。

## 7. 后续修复优先级（本轮未修改产品）

1. 对全局代理 URL 建立实际支持协议的服务端校验，在保存和使用边界拒绝不支持的 scheme，避免“已开启却静默直连”。同一 URL 的不同消费者支持集合需明确，不能只用 `Url::parse` 作为可用性证明。
2. 明确 `proxy_bypass` 的真实契约：接入需要覆盖的 HTTP/WS/Web Access/更新客户端，或移除误导性配置入口；不要继续承诺未实现的绕过能力。
3. 澄清开关范围：Web Access 当前不受 `proxy_enabled` 门控，是 ADR-0031 所述独立 Web Provider `use_proxy` 行为；如果产品要总开关，须先决定并统一契约，不能把现有设计差异直接称为同一 bug。
4. 将多字段代理配置改为一致提交/一致读取，避免独立 PUT 的部分成功与过渡组合。此风险目前仅源码推导，未做失败注入。
