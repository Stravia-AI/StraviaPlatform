# Vendor Wasm 插件设计

本文记录 Vendor Wasm 插件的已确认边界与实现契约。运行代码、构建入口与行为验证共同作为事实来源，未确认的策略不得作为实施依据。

## 已确认的身份与信任

- 一个 Vendor Plugin 软件包实现一个稳定的 Vendor 实现身份；Vendor 身份不由 npm package、Provider Catalog 或协议决定，见 [ADR-0067](../adr/0067-separate-vendor-identity-from-package-and-protocol.md)。
- Vendor 实现拆为 `base` 回退 Vendor 与 `openai-codex`、`xai-grok`、`command-code`、`devin`、`claude-code` 等专属 Vendor（完整列表见下方组件身份映射）。程序默认只内嵌 `base`；专属包仅作为独立 Release 附件发布，由管理员通过本地包导入。`base` 是一个软件包和一个 Vendor 身份，通过多个 Provider Profile 覆盖专属接入之外的全部现有供应商，不是“一个包导出多个 Vendor”。
- Provider Profile 的 `provider_id` 标识供应商接入，`catalog_id` 仅关联目录；两者都不等于已保存 Provider 连接的数据库 UUID。普通 OpenAI 与 xAI API Profile 归 `base`，Codex 与 Grok 分别使用 `openai-codex` 与 `xai-grok` 专属 Profile。
- 既有 `openai/codex` 与 `xai/grok` 连接只迁移供应商 Profile 归属，分别指向 `openai-codex` 与 `xai-grok`；连接 UUID、channel、凭据、Route 和历史保持不变。
- 专属 Profile 对匹配身份下的全部连接、channel 和操作进行整体接管。专属包不存在、缺少能力或 channel、加载失败或执行失败时明确失败；不得按单次能力、channel 或操作回退到 `base`，也不得把两边的声明、网络 origin 或行为合并。
- 管理员为所选插件的连接登录或填写凭据，即授权插件处理该连接的上游秘密；宿主拥有凭据持久化与跨连接隔离，见 [ADR-0068](../adr/0068-trust-vendor-plugins-with-connection-credentials.md)。
- 本文“仅 `base` 默认内嵌”的分发策略取代 [ADR-0067](../adr/0067-separate-vendor-identity-from-package-and-protocol.md) 中五个包均随程序交付的历史描述；ADR 所确定的 Vendor 身份与协议边界不变。

### OAuth 刷新与凭据健康

Core 统一管理已保存 OAuth 连接的刷新，不由各管理入口或 Vendor 分别维护第二套恢复状态：

- 非认证操作在冻结连接快照时检查凭据新鲜度；有可用 refresh token 且到期时间距当前不超过 60 秒，或上次刷新为 `error`、已有 `refreshing` 租约时，先等待共享刷新，再使用新凭据。Core 不为缺失的到期时间新增 TTL 回退，也不为没有 refresh grant 的连接伪造续期。
- 同一 Provider、OAuth 连接与配置代际共享一次在途交换，存储 CAS 租约负责跨 Gateway 的互斥。等待方不得拿旧凭据冒充刷新成功；单个请求取消或超时只结束自己的等待，共享交换仍可完成并保存轮换后的令牌。共享操作有 120 秒时限，超过 125 秒的存储租约才可回收；应用关闭、连接替换与不兼容组件变更仍取消或阻止旧任务写回。
- 暂时刷新失败保留 access token、refresh token、到期时间和连接身份，记录 OAuth `error` / `last_error`，后续请求或后台调度仍可尝试。刷新响应未轮换 refresh token 或返回空值时保留原值。SQLite、PostgreSQL 与 Memory 使用相同状态与活跃租约规则。
- 上游 401 的恢复遵守原组件、连接代际、截止时间与结果提交边界；推理和独立能力使用既有 Target 恢复预算，管理操作的单次 Vendor 调用最多刷新并重发一次。分页发现与独立能力的普通重试沿用本次恢复后的快照，不再次发送旧 token。刷新失败保留真实错误分类；只有明确授权拒绝、不可恢复的已拒凭据或刷新后的凭据仍被拒绝，才按 [ADR-0073](../adr/0073-mark-provider-credential-invalid-on-upstream-auth-rejection.md) 条件标记 Provider 失效。
- 已保存连接的显式 `ConfigValidation` 和 `ConnectionCheck` 不要求启用 Provider，检查不会修改 `is_enabled`；其它非认证操作仍遵守停用状态。
- 额度缓存及守卫证据采用最终成功执行的凭据版本和连接身份；本次刷新可以更新缓存身份，但不能把旧配置、旧账号连接或旧组件身份的查询结果写入新身份。

`OAuth status = error` 是最近一次刷新结果，`Provider credential_status = invalid` 是持久化的授权失效判定，两者不同。后台任务跳过已失效或禁用的 Provider，不以半开探测自动解封。此前已被标记失效的连接，升级后可显式「重新连接」：刷新成功即恢复 `ok`；上游明确撤销 refresh grant 时仍需重新登录，不删除或自动重绑原连接。

宿主生命周期修复需要升级并重启宿主；令牌错误分类和响应解析修复位于各 Wasm 插件，已安装的 Antigravity、Claude Code、Codex、Devin、Grok 等独立插件还须重新构建并导入。仅更新宿主不会替换已安装的独立组件。

### 单入口连接检查

管理面保留一个「测试连接」动作。`POST /api/v1/providers/{id}/test` 无 body 或 `{}` 时检查已保存连接；携带 `configuration` 时检查当前草稿，包括未保存的 Base URL、配置、凭据、代理选择和 Ready 状态的 staged OAuth。空密钥沿用同一已保存连接的值。草稿不保存 Provider、不绑定 OAuth、不写原连接的测试状态或凭据健康；OAuth 返回值不能扩张临时执行的网络授权。

临时执行与已保存连接一样，由宿主在 `operation_metadata.vendor_profile` 中传入本次取得的有效 Profile。动态目录 Profile 不一定存在于 Base 的编译时清单，草稿检查和配置校验不能因此丢失实现路由、误报 `unsupported`；此元数据不扩张网络授权。

`ConnectionCheck` 是每个插件必须实现的操作，不属于可选 `Capability`，不出现在插件支持功能列表中。Rust SDK 的 `VendorGuest::check_connection` 没有默认实现；统一分发在确认 Profile 和 channel 身份后直接调用该方法，不再经过可选能力准入或 `execute`。已安装且仍声明 `connection_check` 能力的独立组件须使用新 SDK 重建并重新导入；WIT 操作布局与版本不变。

Core 先复用字段、协议、地址及 guest `ConfigValidation` 校验，再执行必需的 `ConnectionCheck`。失败使用 `configuration`、`transport`、`upstream`、`response` 阶段和本地化 code；宿主不把原始上游 body 或插件执行诊断作为错误展示。上游拒绝请求属于 `upstream`，成功响应无法按协议解析属于 `response`。插件实现确认没有安全只读探针时，显式返回中性 `unsupported`，不能记作连接失败，也不能以配置校验或静态模型冒充连通。

成功结果只声明实际证据范围：`endpoint` 证明访问过配置端点，`credentials` 证明固定账号端点接受凭据，`endpoint_and_credentials` 同时证明两者。公开或鉴权语义未知的模型目录只能证明端点；仅携带认证头不能证明凭据已被接受。Vertex 自定义端点配合直接 access token 同样只能证明端点，不能清除凭据失效标记。只有明确的凭据验证成功才能恢复凭据健康；已保存检查按最终执行的 Provider / OAuth 版本及插件 publication fence 条件写回。

准备阶段的正常 OAuth 刷新可以推进凭据健康版本。若此时保存的连接配置与已校验快照不一致，或结果提交时版本失配，检查返回 `cancelled`，不覆盖新状态；不能仅因正常刷新推进了健康版本就误取消检查。

探针必须真实读取 HTTP 状态并验证响应结构，不生成内容、同步模型、执行 onboarding、`AssignModel` 或写插件私有状态。宿主拒绝没有实际 HTTP 2xx 响应的静态成功，以及检查期间的私有状态写入和生成事件。HTTP POST 本身不表示写操作，供应商只读 RPC 可以使用 POST。Base 中没有安全既有探针的 Bedrock、原生 Vertex 和 Watsonx 明确不支持；其它实现使用真实目录或账号只读查询。已保存 OAuth 连接仍遵守前述新鲜度和单次认证恢复预算，不新增普通重试。耗时表示整个检查用时，不是推理延迟。

### 组件身份映射

| crate | Vendor ID / kind | Provider Profile |
| --- | --- | --- |
| `stravia-vendor-base` | `base` / `fallback` | 除下列专属 Profile 外的全部现有供应商；`openai`、`xai`、`anthropic` 仅保留普通 API channel。 |
| `stravia-vendor-codex` | `openai-codex` / `dedicated` | `provider_id = openai-codex`、`catalog_id = openai`、channel `codex`。 |
| `stravia-vendor-grok` | `xai-grok` / `dedicated` | `provider_id = xai-grok`、`catalog_id = xai`、channel `grok`。 |
| `stravia-vendor-command-code` | `command-code` / `dedicated` | `provider_id = command-code`、`catalog_id = commandcode`、channel `default`；目录名称为 `CommandCode`。 |
| `stravia-vendor-devin` | `devin` / `dedicated` | `provider_id = devin`、`catalog_id = devin`、channel `devin`。 |
| `stravia-vendor-claudecode` | `claude-code` / `dedicated` | `provider_id = claude-code`、`catalog_id = anthropic`、channel `oauth`。 |
| `stravia-vendor-antigravity` | `antigravity` / `dedicated` | `provider_id = antigravity`、无目录映射、channel `oauth`；模型与额度来自登录账号。 |
| `stravia-vendor-opencode-free` | `opencode-free` / `dedicated` | `provider_id = opencode-free`、`catalog_id = opencode`、channel `default`。 |

### 供应商选择器与来源

管理接口 `GET /api/v1/vendors` 返回完整有效的 Provider 元数据，供已有连接读取配置；新建连接使用 `GET /api/v1/vendors?selectable=true` 获取应用替代规则后的候选列表，两者不共用查询缓存。原描述符字段保持扁平布局，新增 `plugin: { id, name }`，由宿主从实际加载的软件包身份及名称生成。卡片显示认证方式；来源为内置 `base` 的卡片不标注来源，外部插件的卡片以拼图图标标记，提示显示插件名称和插件 ID。连接配置步骤显示目录标识、来源插件名称和插件 ID；搜索同时覆盖展示名称、目录标识、执行身份及插件来源。所有卡片图标统一渲染为单色，不保留品牌自带颜色。

Core 以「目录身份 + channel ID」处理选择器中的替代关系：目录身份优先使用 `catalog_id`，未声明时使用 `provider_id`。外部插件声明同一入口时，仅隐藏 `base` 对应 channel 的卡片，并沿用该 base Profile 的展示名称；未匹配的 base channel 继续可选。不同外部插件即使声明相同目录入口也各自保留，不能互相覆盖；不同 channel 的 API 与 Codex／OAuth 接入不合并。卸载外部插件后，原 base 入口重新可选。相同执行身份的已安装包接管及 retired Profile 隐藏规则保持不变。

上述替代仅作用于新建连接的选择列表，不改变运行时描述符、执行解析或已保存连接。WebUI 始终使用所选 Profile 的 `provider_id` 和 channel 创建连接，不能拿目录 ID 分发。例如 Command Code 展示 `CommandCode`／`commandcode`，来源为 `Command Code (command-code)`，执行身份仍为 `command-code`。升级宿主后还须重新构建并导入独立插件，才能更新旧组件自带的目录标识；不迁移连接 UUID、凭据、Route 或历史。

### 客户端身份版本

客户端身份固定在插件源码中，不在请求期间查询发行源。版本采用官方已发布渠道，不将预发布版、`Unreleased` 条目、IDE/hub 或独立 SDK 的最新版本冒充对应 CLI。版本字段不是插件包版本，也不改变 OAuth 权限或协议版本。

| 接入 | 当前默认客户端身份 | 官方版本来源 |
| --- | --- | --- |
| Codex | `0.167.0`；originator `codex-tui` | [官方 npm latest](https://registry.npmjs.org/@openai%2fcodex/latest) |
| Claude Code | `2.1.319`；Stainless SDK `0.128.0` | 本次隔离采集的官方 Linux native 客户端，见 `shape/fixtures/claude-code.json`；既有协议研究仍以各自记录的版本为准 |
| Grok CLI | `1.0.52` | [官方 stable 指针](https://x.ai/cli/stable) |
| Command Code | `1.82.1` | [官方 npm latest](https://registry.npmjs.org/command-code/latest) |
| Devin CLI / chisel | `3000.11.3` | [官方 stable changelog](https://docs.devin.ai/cli/changelog/stable) |
| OpenCode Zen Free | `1.18.25`；provider-utils `4.0.23`、Bun `1.3.14` | 本机官方 CLI 隔离抓包与真实 Wasm 实调；[固定发行源码](https://github.com/anomalyco/opencode/tree/cb7d8b2f5e44876ef98b661dc10590c915af3a9f) |
| Antigravity CLI | `1.5.2`，`cl=1015175089`，保留 `/cli/` 身份路径 | 本次隔离采集的官方 Linux/amd64 consumer 推理连接，见 `shape/fixtures/antigravity-inference.json`；[官方 CLI 更新 manifest](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json) |
| base / GitHub Copilot 额度 | `editor-version: vscode/1.141.1` | [微软 stable 更新 API](https://update.code.visualstudio.com/api/update/win32-x64/stable/latest) |

Claude Code 当前默认版本为 2.1.319，使用普通交互式身份 `claude-cli/2.1.319 (external, cli)`。当前黄金样本来自真实 PTY 的主推理请求，同一 socket 关联 TLS 与 HTTP，已替换此前 `-p`／`sdk-cli` 样本；其余请求在隔离环境中收到本地 401，因此只证明无远端 GrowthBook 配置时的默认行为，不证明账号分配的灰度功能。Linux/x64、Node `v26.12.1` 与 Stainless SDK `0.128.0` 是所模拟的请求身份，不声称 Wasm 在运行 Node。此前 2.1.317 的真实账号、多轮与 Read 工具结果保留为[历史协议研究](../research/claude-code-message-threads.md)，不重新标成 2.1.319 的上游接受证据。OpenCode 保留已捕获的完整 UA 组合，并选择对应 Bun 1.3.14 形态；此前 `1.19.0` 组合被拒，不能仅凭 npm latest 推定可用。Command Code 的额度、初始化与推理使用同一版本常量。

已安装的独立插件须重新构建并导入才能更新身份；仅升级宿主不会替换旧组件。Claude Code 的已声明 `client_version` 覆盖仍优先于默认值；OpenCode 不再提供任意 `userAgent` 覆盖，旧未知选项不会进入出站请求。历史协议研究及密码学向量保留原始核对版本，不因更新身份而重标来源；离线组件与请求边界验证不能证明真实账号的版本门控、模型可用性或订阅资格。

### 会话与请求 ID

以下结合固定版本源码、既有授权实测和 2026-11-07 的隔离交互式采集；来源日期与客户端版本不混用。本仓库有 10 个 Vendor 包，没有 WorkBuddy；不套用其 root、UUIDv7 或 baggage profile。普通 API、CLI 推理、compact、认证、发现和额度分别保留自己的请求 profile，不按字段名称推定生命周期。

可信来源只有 SDK `ProviderSnapshot.operation_metadata` 中由宿主生产的作用域键：

- `session_affinity`：Principal + Target + 本地 Generation Chain／Agent Run 根；同一链路各轮、内部调用与压缩共享。
- `transport_affinity`：Principal + Target + 会话 fingerprint；没有会话时回退本地链路根或 Model Turn。parent、fork、context 使用各自宿主 scoped affinity。
- 安装身份、逻辑用户轮、已知计数与上下文窗口由下文 State 1.x 契约提供；重试、工具续接、fork 与压缩沿来源证明继承，不重新猜测。
- 客户端 headers、canonical request/session/turn 标签和 Provider options 不得覆盖这些身份。没有可信来源的可选事实省略，必要事实显式报错，不拿 session/root 冒充 request/message。trace/span、真实 ACP connection ID 与认证凭据不参与会话 ID 转换。

| 插件及核对版本 | 字段、表示与生命周期 | 官方证据及适用范围 |
| --- | --- | --- |
| base / OpenRouter API `1.0.0` | 仅 OpenRouter 的 `x-session-id` 为完整源稳定派生的 64 位小写 hex；无源省略。其他普通 API 不新增 CLI 会话头。 | [官方 OpenAPI](https://openrouter.ai/openapi.json) 仅要求 session string 最多 256 字符，无 UUID 限制；[官方 sticky routing 说明](https://openrouter.ai/blog/tutorials/prompt-caching-sticky-routing/)要求会话内稳定。Chat／Responses 的 routing 语义不外推到其他端点；body `session_id` 优先于 header，因此不允许客户端扩展覆盖宿主值。 |
| Codex `0.167.0` | session=thread，由可信 conversation 身份稳定映射；`x-client-request-id=thread-id`，并非每次 HTTP 新建。turn 取宿主逻辑用户轮的稳定 UUIDv7 位格式投影与真实起始时间，工具循环和重试复用；根线程的 `root_turn_id` 即本 turn，无可信父 turn 则省略。window 为已证明的 `thread:N`，context-window 取独立宿主窗口 UUID。安装 UUID 写入 body metadata，不追加安装 ID header。 | 官方 tag [`rust-v0.167.0`](https://github.com/openai/codex/tree/rust-v0.167.0)：[ThreadId](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/protocol/src/thread_id.rs)、[HTTP](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/codex-api/src/endpoint/responses.rs)、[WS／压缩](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/core/src/client.rs)及 [turn metadata](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/core/src/responses_metadata.rs)；独立 PTY TUI 的 HTTP 和 WS 均采集两次。Search／hosted image 走 Responses，不推广到独立 Images API。 |
| Claude Code `2.1.319` / SDK `0.128.0` | session/parent 为 UUIDv4 位格式；session header 与 `metadata.user_id.session_id` 同值。prompt 为会话 CAS 保存的随机 UUIDv4，同一可信用户轮的工具循环和重试复用；每次 HTTP 尝试另建 UUIDv4 `x-client-request-id`。device 为安装 UUID 派生的稳定 64 hex，account 来自 OAuth 身份。 | [当前交互式捕获](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/claude-code.json)与此前多轮证据分别保留版本。`cc_prev_req` 取上一成功响应的 `request-id`，不是客户端 UUID；`previous_message_id` 取上游 `msg_`，`toolu_` 原样回传。输入 Messages 不强加 CLI 本地 `msg_id`，HTTP ID 的推理生命周期不外推到 models/usage。 |
| Grok CLI `1.0.52` | `x-grok-conv-id` 与 `x-grok-session-id` 同一 UUIDv7 位格式；缺 scope 省略。`x-grok-req-id` 每调用独立新建 UUIDv4。 | [官方 1.0.52 发行物](https://x.ai/cli/grok-1.0.52-linux-x86_64.gz)确认版本及头名；UUIDv7 默认工厂与 UUIDv4 RequestId 来自[固定公开源码](https://github.com/xai-org/grok-build/tree/9684fa3cdbf2995e30ea8b9b637f1db008f144fc)，该源码实际为 **1.0.10**，不能冒称已证明 1.0.52 全部生成算法。其 compact/recap 等路径允许带标签 ID，不能推定服务端所有接口强制裸 UUID。当前 stable 指针为 1.0.67，本次未升级固定身份。 |
| Command Code `1.82.1` | trusted session 映射 UUIDv4 位格式，`x-session-id=body.threadId`。无 scope 保留既有 UUIDv4／TTL 新建生命周期；独立 telemetry `sess_`、trace/span 与 fingerprint 不变。 | [官方 npm 产物](https://registry.npmjs.org/command-code/-/command-code-1.82.1.tgz) `newSession` 使用 `randomUUID`，`toWireThreadId` 使用 `z.uuid()` 校验；此为客户端校验，不是服务端接受证明。telemetry session 不是 thread。 |
| Devin `3000.11.3` | trajectory/cascade 按两个既有不同语义映射为 UUIDv4 位格式，不再以 token 参与派生。每次编码 message 仍新建 UUIDv4；step 推进、AssignModel 的 cascade/JWT 绑定不变。 | [官方 manifest](https://static.devin.ai/cli/3000.11.3/manifest.json)及[固定发行物](https://static.devin.ai/cli/3000.11.3/devin-3000.11.3-x86_64-unknown-linux.tar.gz)摘要匹配，但 stripped 二进制不足恢复精确字段生成／validator。**v4 只是保留既有 codec 形态，官方字段版本尚未证实**；不把第三方 3000.10.x 编码证据重标为官方 3000.11.3 契约。 |
| Antigravity CLI `1.5.1`；session 算法另核对 `1.3.5` | `sessionId` 为完整 UTF-8 的 FNV-1a64 按 signed int64 输出十进制，无 scope 省略。文本 `requestId` 保留 `agent/UUIDv4` fallback；独立生图使用 `image_gen/<毫秒时间戳>/<UUIDv4>/1`，不模拟 CLI 的共享操作树或步数。 | 官方 agy 1.5.1 生图抓包确认 `image_gen/<时间戳>/<操作 UUID>/<步骤>`；[官方 1.3.5 Linux 发行物](https://github.com/google-antigravity/antigravity-cli/releases/download/1.3.5/agy_cli_linux_x64.tar.gz)反汇编确认 `getSessionID` 的 `FormatInt` 及 `computeRequestID` fallback。上层输入结构的字段准确名称／恢复作用域尚未完全恢复，不声称服务端强制此算法。 |
| OpenCode Zen Free `1.18.25` | 推理 session 为 `ses_` + 12 lowerhex + 14 base62，取宿主持久会话身份；`x-opencode-request` 的 `msg_` 取独立用户输入身份与真实创建时间。工具续接和重试复用用户消息 ID，新用户输入更新；重启保留关联，不从 session 或 HTTP UUID 猜测。发现与连接检查等辅助操作仍按真实时间新建 ID。 | 固定源码 [`cb7d8b2f`](https://github.com/anomalyco/opencode/tree/cb7d8b2f5e44876ef98b661dc10590c915af3a9f)：[默认算法](https://github.com/anomalyco/opencode/blob/cb7d8b2f5e44876ef98b661dc10590c915af3a9f/packages/schema/src/identifier.ts)为 time/counter/random，客户端 session/message 类型只检查 `ses`／`msg` 前缀；[请求头](https://github.com/anomalyco/opencode/blob/cb7d8b2f5e44876ef98b661dc10590c915af3a9f/packages/opencode/src/session/llm/request.ts#L190-L194)取 `input.sessionID`／`input.user.id`。推理缺少宿主可信身份或时间时明确失败，不新增另一套持久映射。公开 Zen handler 无时间解码证据，不能替代当前私有网关校验。 |

#### 固定映射契约

源字符串以**完整原文 UTF-8**进入散列：不 trim、截断、改大小写、Unicode 归一化或 hex decode。缺失、空串、类型错误均视为无可信源，按上表省略或原生新建；仅含空白的非空字符串仍是不同身份。控制字符只参与散列，不原样进入 header。没有内存／持久化 source→ID 映射表。

除已发布的 Claude 冒号 namespace 与官方 FNV 算法外，下表 `\0` 都是**真实 NUL 字节 `0x00`**，不是两个字符。UUID 映射取 `SHA256(namespace || source.UTF8)[0..16]`，`bytes[6]=(bytes[6]&0x0f)|(version<<4)`、`bytes[8]=(bytes[8]&0x3f)|0x80`，使用 RFC UUID 网络字节序、小写带横线表示，不采用混合端序。算法属于稳定身份契约，不能随版本更换。

| 字段 | 固定 namespace／算法 | 源 `user-run-a` 的参考输出 |
| --- | --- | --- |
| OpenRouter session | `stravia-openrouter-session-v1\0`；全部 SHA256 输出为 64 lowerhex | `39e55f82ef4ff56ea4e8dd4439d2d71d2b657085d36a77cb65419d0ddac211fc` |
| Codex session/thread | `openai-codex:conversation\0` + `client_conversation_id`；无 conversation 时才使用既有 `openai-codex\0` + transport affinity | 后者对 `user-run-a` 的结果为 `62b0511e-90cd-7ce2-a863-774b8cc15be1` |
| Codex logical turn | `openai-codex:logical-turn\0` + `logical_turn_id`；UUIDv7 位格式 | 独立于 thread，与宿主逻辑轮稳定绑定 |
| Codex context window | 直接使用获准的 `context_window_id` UUID | 不复用 thread，不从内容或 transport affinity 猜窗口 |
| Claude session/parent | 保留既有 `claude-code:`；UUIDv4 位格式 | `0bc95508-ec2f-4603-95db-de73350b2a28` |
| Grok conversation/session | `stravia:xai-grok:session:v1\0`；UUIDv7 位格式 | `ea3a3e91-202f-7c6a-a14a-40f17619ecee` |
| Command session/thread | `stravia:command-code:session:v1\0`；UUIDv4 位格式 | `e779caf0-834a-4e90-8ae8-d66a0269bb3f` |
| Devin trajectory | `stravia:devin:trajectory:v1\0`；UUIDv4 位格式 | `b2ff88c8-446c-4a64-bf89-230532a0c9d4` |
| Devin cascade | `stravia:devin:cascade:v1\0`；UUIDv4 位格式 | `fcde4a4a-da9f-4a77-9e56-c8b5bec23dad` |
| OpenCode session | `stravia/opencode-free/session\0`；digest[0..6] 为 12 hex，digest[6..20] 逐字节 `%62`，字母表 `0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz` | `ses_7625576406f7WczdWZpXXOdKpd` |
| Antigravity session | FNV-1a64 初值 `0xcbf29ce484222325`、乘数 `0x100000001b3`，逐 UTF-8 byte xor/multiply、64-bit wrap，转 signed int64 十进制 | `8470453353509564100` |

Devin 的两个固定 namespace 区分原有两种独立身份，不是字段级随机盐。session/thread 本来同义的字段使用同一内容；父身份与此前父会话使用同一映射。没有设备、路径、时钟、随机数或认证凭据参与已有宿主 ID 的转换。已有安装／device 身份和 Devin step、Command 初始化等必要持久状态不属于 source→ID 映射，继续按各自生命周期保存。

**明确限制与首次切换：**

- 给摘要设置 v7 位仅是 **UUIDv7 位格式**，不是带真实创建时间的 UUIDv7 生成，也不是官方提供的确定性算法。Codex/Grok 不具真实时间排序语义；OpenCode 稳定 `ses_` 的前 12 hex 不再是创建时间，不能用于原生时间解码／排序。其独立 `msg_` 与无亲和 `ses_` 的真实时间新建路径不变。FNV-1a64 是非加密 64 位散列，存在理论碰撞风险。
- Codex 必须取得可信逻辑轮、窗口 UUID 和窗口序号；不会因缺少历史证明而固定输出 `thread:0`。Responses Lite 的首次记录身份由 journal 保留，失败 admission、checkpoint、fork 与压缩依照宿主来源关联，不用 canonical 自报标签跨操作命中状态。
- 本次增加安装身份与持久轮次／窗口来源；SQLite 与 PostgreSQL 应用同等迁移。旧记录没有相应证明时保持未知，不根据压缩后的剩余消息补写计数。已有的认证与网络授权边界不变；更换 Vendor 请求身份可能重新建立上游缓存亲和与 assignment。
- 独立插件必须重新构建并导入；base 随宿主更新。公开源码与隔离捕获证明构造及字段生命周期，**不证明私有服务端接受任意确定性值**。Devin fingerprint 已还原为排序设备材料的 SHA-512，再以 session token、纳秒时间和随机 nonce 执行原生 AES-256-GCM 封装；设备材料从持久安装 UUID 派生虚拟 MAC／序列号／用户名，不采集真实硬件。Command 设备 fingerprint 也由安装身份稳定派生，不宣称与某台官方 CLI 主机的硬件值相同。

本地验证加载实际构建的 10 个 Wasm，经 loopback HTTP/TCP 捕获请求，覆盖参考向量、同源重载与独立进程稳定、root/session 隔离、Unicode/CRLF/NUL、空串／错误类型／缺失源、客户端注入与请求头／body 关联；独立 request/message 仍按各自 profile 新建。响应由本地接收器合成，不访问真实上游，不代表账号资格、订阅门控或服务端格式接受证明。

### 参数清单的核对证据与边界

|接入|核对依据|实际覆盖与限制|
|---|---|---|
|Codex|官方 tag `rust-v0.167.0` 的 [Responses／WS 请求结构](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/codex-api/src/common.rs)、[会话 metadata](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/core/src/responses_metadata.rs)、[请求与压缩构造](https://github.com/openai/codex/blob/rust-v0.167.0/codex-rs/core/src/client.rs)|经授权，以本机官方 TUI `0.167.0` 与真实账号经 HTTPS 代理捕获 HTTP `/responses`（Responses Lite）、WS `response.create`（含 `generate:false` 预热与续接增量）、`/models` 与 `wham/usage`；压缩、search、image 的清单来自源码，未实际触发抓包。|
|Claude Code|官方 [Linux native `2.1.317` 发行包](https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-2.1.317.tgz)、[Messages Threads 文档](https://platform.claude.com/docs/en/build-with-claude/message-threads)与真实授权抓包|SDK、交互式默认 system、交互式自定义 system 均完成真实多轮请求；Haiku 5.5 的实际 Wasm 验证包含 Read 调用与返回、跨轮回忆、create/continue。不能把 SDK 公共参数全集、CLI 私有权限上下文或灰度遥测直接放行给订阅端点。|
|Command Code|官方 [npm `1.82.1`](https://registry.npmjs.org/command-code/1.82.1) 的 `createModelClient`|源码确认 model、messages、tools、system、max_tokens、stream、temperature、reasoning_effort 与 promptCache；不发送该工具没有产出的 tool_choice／parallel_tool_calls。发行 tarball SHA-1 为 `0484a5dda887bec30a349db0b84a5a8933df69cd`，与 registry 一致；本轮未实际运行其账号推理。|
|Grok|官方 [Grok Build 固定提交](https://github.com/xai-org/grok-build/tree/9684fa3cdbf2995e30ea8b9b637f1db008f144fc)的 Responses、sampler、tool override 实现|按源码重建 body、搜索控制与 `x-grok-*` 会话头；本轮未完成官方 CLI 账号抓包。|
|Devin|[OMP `v18.12.6` 的 `buildChatMessagePrompts`](https://github.com/can1357/oh-my-pi/blob/v18.12.6/packages/ai/src/providers/devin.ts)、[官方 CLI `3000.11.3` manifest](https://static.devin.ai/cli/3000.11.3/manifest.json) 与授权实调捕获|2026-10-26 使用隔离配置运行官方 CLI，捕获 `deepseek-v4-1-flash-1017-max` 的工具调用、返回与最终正文；无签名思考在续接请求中保留为 `#11`，不降级到 `#3`。修复后的实际 Wasm 经隔离 Stravia HTTP 服务完成同模型的读文件工具循环并返回 `SUM=42`；不据此宣称所有模型或参数均获验证。|
|OpenCode Zen Free|官方 `1.18.25` 的隔离请求捕获、[原生 ID 算法](https://github.com/anomalyco/opencode/blob/cb7d8b2f5e44876ef98b661dc10590c915af3a9f/packages/schema/src/identifier.ts)与匿名单变量探测|捕获完整 UA、会话头和工具输入 schema；实际 Wasm 在 `/inference/v1` 对 `big-pickle` 返回 200 与准确探测文本。不能外推为所有免费模型、其他协议或未来门控的支持证明。|
|其他插件|各自既有固定官方协议／工具源码、模型目录与 OMP 实现|通过实际 Wasm 合同验证投影与流式解析，不把离线 fixture 当作账号资格证明。|

除表中明确注明的授权真实调用外，上述 CLI 捕获使用合成凭据、隔离配置与 loopback 接收器，返回受控错误，不执行模型工具，不产生官方服务调用。它们证明工具构造出的参数形态，不证明上游接受任意字段取值；授权的真实调用另行验证成功终态。Devin 的真实调用使用只读生产数据库取得的连接凭据、临时 CLI 配置与临时 Stravia 数据库，不修改生产连接或部署；已安装的独立 Devin 插件须重建并重新导入才能采用无签名思考修复。真实 Codex HTTP 对同一可用模型的 `stream:false` 返回 400／`stream must be set to true`，改为 `stream:true` 后返回 200 并完成。插件因此对推理与独立压缩固定使用官方流式 wire；客户端可继续选择非流式响应，由宿主汇总结果，canonical 的交付选择不被改写。独立压缩是末尾带 `compaction_trigger` 的流式 `/responses`，不调用 `/responses/compact`。

### Command Code 模型发现

模型获取、订阅筛选与规格解析全部属于 `stravia-vendor-command-code`，Core 和 WebUI 只消费统一的 `DiscoveredModel`，不维护 Command Code 的套餐或字段分支。

- 插件在连接 origin 下读取 `/alpha/whoami`，以返回的 `org.id` 查询 `/alpha/billing/subscriptions?orgId=...`，再读取 `/provider/v1/models?view=client`。客户端目录是全量目录，不是账号已获授权的模型列表；裸 `/provider/v1/models` 不提供完整的模型能力与订阅要求。目录与订阅规则依据[官方 CLI 1.87.1 发行物](https://unpkg.com/command-code@1.87.1/dist/cli.mjs)，不改变上表中的推理客户端身份版本。
- 只有 `active`、`trialing`、`past_due` 状态提供当前订阅身份。插件按官方套餐顺序与每行的 `minimumPlan` 筛选，排除隐藏模型；未提供 `minimumPlan` 时使用官方 `opensource`／`premium` 分类规则，显式 `null` 不授予订阅访问。这里只列出订阅权益，不因充值或赠送余额解锁额外付费模型。无有效订阅、未知套餐、订阅查询失败或筛选后没有模型时明确报错，不退回全量目录或发布一个空库存。
- 插件将 `context_length` 和 `inputModalities` 转为统一上下文与模态规格，将 `reasoningEfforts` 转为 `reasoning_efforts`。目录的 `off` 在统一规格中表示为 `none`，供 Core 生成规范的 `off` 档映射；私有 codec 发送时再还原为 CLI 原生的 `reasoning_effort: "off"`。显式无 effort 声明保持为空列表，缺失声明保持未知，不硬编码具体型号的档位。
- `static_models` 只能进一步缩小当前订阅允许的目录集合，不能添加未知或更高套餐的型号，也不绕过账号查询。显式 `models_source` 仍指定目录地址，但响应必须包含客户端目录所需的订阅要求；插件不把仅含 ID 的列表当作订阅权限证明。

升级后重新构建并导入独立插件，再同步模型。未固定的规格字段跟随来源更新；管理员修改的字段保持固定，可逐字段恢复同步，不自动覆盖固定值，也不删除已有 Route 或 Target。

### OpenCode Zen Free 准入限制

默认 Base URL 使用当前[官方文档](https://opencode.ai/v2/docs/console/models/)的 `https://opencode.ai/inference/v1`。2026-10-15 的修复后 Wasm 实调在该入口对 `big-pickle` 返回 HTTP 200，并完成准确文本探测；旧 `/zen/v1` 请求仍收到 403。此前失败请求只改入口也仍被拒，说明不能把入口迁移当作 UA、会话 ID 和工具 schema 修复的替代。重新构建并导入插件后，手动固定旧 Base URL 的已有连接还须改为当前地址；插件不静默重写自定义地址。

免费请求使用已核验的官方完整 UA、固定原生 `ses_` 表示与独立时间编码的 `msg_`、流式 wire 和 `tool_choice: auto`。只有缺失的 `bash`／`read` 才补兼容声明，输入字段采用官方工具形态，不能再使用空 `properties`；客户端同名工具与已有指令保持原值，仅对补出的名字追加不可调用指令。这里没有工具执行器，不伪造工具结果；系统指令是模型约束，不是强制执行隔离。目标原生不支持的 tool-choice 模式不能保证原语义。

宿主的主体／Target／链路作用域亲和键按上方「会话与请求 ID」契约确定性编码为原生 `ses_` 表示，不读写私有映射表；完整源参与散列，不只截取源前缀。稳定表示不含真实创建时间；独立 `msg_` 与无亲和 `ses_` 仍按官方时间／随机新建。升级后旧缓存映射不再读取，原生 session 首次切换；不迁移历史或改写存储 schema，每次仍发送完整消息。

`/models` 可读不代表推理获准；[官方维护者仍限定免费层在 OpenCode 内使用](https://github.com/anomalyco/opencode/issues/49621#issuecomment-5723383322)。本次成功不构成第三方接入承诺，模型、配额与门控仍可能变化。插件保留原生拒绝，不轮换 IP、不盲重试、不改用其他模型或另一个 Agent 后端。来源和受控对照见[研究记录](../research/opencode-free-models.md#343-本仓库回归修复2026-10-15官方-cli-捕获与真实-wasm)。

### Codex 额度单位

`openai-codex` 的 `credits.balance` 与 `spend_control.individual_limit` 金额按官方客户端语义使用 credits，输出现有 `unit="credits"` 且不声明 currency；保留数值和小数，不将点数当作 USD，不推测兑换率或缩放。WebUI 沿用通用额度单位本地化，中文显示「点」、英文显示 `credits`。更新独立 Codex 组件后刷新额度，既有 guard key、认证与数据库 schema 不变。固定官方源码证据、数值边界与验证见 [Codex 额度字段单位研究](../research/codex-credit-units.md)。

### Devin 耗尽窗口

Devin 的 `GetUserStatus` 在 `PlanStatus` 中报告每日、每周剩余百分比和重置时间。按照 [Protobuf 的隐式字段存在性规则](https://protobuf.dev/programming-guides/field_presence/)，剩余百分比为零时可以省略对应字段；只要上游仍报告该窗口的非零重置时间，插件就保留窗口，输出已用 `100%`、耗尽状态和原重置时间。明确编码的零值也保留；比例和非零重置时间都未报告时，不凭空生成额度窗口。

修复位于独立 `devin` 插件，Desktop 与 Server 共用该解析行为。已安装实例需要重新构建并导入 Devin Wasm 包，再刷新额度；单独更新宿主不会替换已安装插件。既有 `daily`、`weekly` guard key、凭据、路由和数据库 schema 不变。

### 停止维护的专属插件

Muse Code（`muse-code`）与 Kilo（`kilo`）专属插件已停止维护，源码、独立组件构建与 Release 附件已移除。`base` 中普通 Meta 与 Kilo Gateway 的 API-key 目录接入不受影响；它们不是原专属插件的订阅／设备登录替代实现，也不会接管原插件连接。

已经手动安装的独立 Wasm 不会因宿主升级自动卸载；既有连接、凭据、Route 和历史数据不自动删除或迁移。需要停用时，管理员先停用或调整关联 Route，再在插件管理页卸载旧包。


### Cline Pass 接入

Cline Pass 不再是专属插件。`cline-pass` 是 `base` 的目录 Provider Profile，随内嵌供应商清单注册，推理复用标准 OpenAI-compatible codec；没有独立的 Wasm 包，也不发布专属 Release 附件。

1. 添加 **Cline Pass** Provider，填写 Cline Pass API Key。默认地址为 `https://api.cline.bot/api/v1`；凭据通过连接范围的 `apiKey` 字段保存并使用 Bearer 认证。
2. 同步模型并绑定到 Route。模型集合按 `cline-pass` Profile 的目录 scope 或管理员显式配置确定；未发布的规格不猜测，沿用现有 Canonical Model 元数据补全机制。

额度监控读取连接地址下的 `/users/me/plan/usage-limits`，呈现上游报告的五小时、每周、每月等窗口及重置时间。base 不提供独立账号池、渠道钉住或自动重试；多个账号分别保存为 Provider，调度与故障切换沿用 Route 策略。

历史手动安装的 `clinepass` 专属插件不会因本次变更自动卸载，其 `clinepass` Vendor 身份下的存量连接仍归旧插件执行，不自动迁移到 base。迁移时管理员新建 `cline-pass` Provider、重新绑定 Route，然后在供应商插件页面手动卸载旧插件。base 的 `cline-pass` 使用标准 OpenAI-compatible 接入，不复刻旧插件对上游响应 `data` 包装与 reasoning 字段优先级的私有处理。

### Anthropic 接入

base 的 Anthropic Profile 仅提供 `default` API-key 通道；旧的 `anthropic/claude-code` 订阅 OAuth 通道已移除，已保存的旧通道连接、凭据、Route 绑定与历史不自动删除或迁移，执行时明确报告不可用。Claude Code 作为 Connect Client 接入 Stravia 的能力不受影响。

Claude Pro/Max 订阅由专属插件 `claude-code`（crate `stravia-vendor-claudecode`，channel `oauth`）提供，需从 Release 附件本地导入。旧 `anthropic/claude-code` 连接不会自动归入该插件；改用订阅时新建 `claude-code` 连接、完成登录并重新绑定 Route。

- 登录：授权码 + PKCE，授权页 `https://claude.ai/oauth/authorize`，回调 `http://localhost:54545/callback`（备用 54546，手动粘贴回调 URL 使用 54547）；claude.ai 授权提交会拒绝宿主默认 state，插件自行生成 16 字节小写 hex state 并交宿主做回调校验，因此该插件要求宿主支持插件自带 state；令牌交换与刷新走 `https://api.anthropic.com/v1/oauth/token`。令牌响应缺少账号身份时，向 `/api/claude_cli/bootstrap` 补取账号与组织；该补取失败不影响登录。刷新沿用已保存的账号身份；出站 device 身份改由持久安装 UUID 派生，不因重新登录或刷新重建。
- 推理以官方 2.1.319 的交互式主请求为基准：`POST /v1/messages?beta=true`，CLI User-Agent、Stainless 与 `anthropic-beta` 头，始终流式。创建请求的 `system[0]` 为 `x-anthropic-billing-header`，身份块为 `You are Claude Code, Anthropic's official CLI for Claude.`；保留调用者自己的 system 和工具，不复制 CLI 的环境提示词或自动权限评估上下文。`cc_entrypoint=cli`、`cc_turn_origin=human`；用户轮位置由宿主可信 admission／checkpoint 提供，工具返回不另计一轮，压缩后不从剩余消息猜测。`metadata.user_id` 由安装、OAuth 与宿主 scoped 会话身份重建，不能被客户端自报值覆盖。默认版本为 `2.1.319`，既有高级设置 `client_version` 仍可覆盖版本，但不改变此协议 profile。
- 自定义工具保留 OMP 式 `_` 隔离：声明、`tool_choice`、上下文清理的 `exclude_tools`、历史 `tool_use` 和工具结果中的 `tool_reference` 同步映射；流式与最终响应只剥离一层后交付客户端。原名 `_read` 发为 `__read`，避免与 `read` 碰撞；`web_search`、`code_execution`、`text_editor`、`computer` 按大小写不敏感匹配豁免，原始拼写不变。参数、schema、正文及工具 ID 不改写。此隔离是有意保留的客户端策略，不是官方 CLI 抓包中的命名规则。
- `x-claude-code-sandy-plover` 使用会话工具历史：工具解析成功后同时记录请求名与解析后的名称并去重；找不到工具时不登记解析结果，但调用方随后提交的历史仍可贡献该请求名。因此本地 `_` 隔离下，一次已声明的 `lookup` 交接可记录 `_lookup` 与 `lookup`；只从历史恢复时不虚构解析结果。依据是官方 `@anthropic-ai/claude-code-linux-x64@2.1.319` 内嵌源码：`eFi` 记录两个名称，其调用位于工具不存在分支返回之后；`$H` 从历史提取名称，`D.record` 去重。此映射不代表官方 CLI 自带 `_` 隔离。
- 计费头的版本指纹使用首条用户文本的 UTF-16 零基索引 4、7、20 处的码元（缺位为 `0`）、固定盐与版本计算 SHA-256，取前三个 hex。`cch` 是最终序列化字节的 XXH64 低 20 位（五个小写 hex，seed `0x4d659218e32a3268`），计算时自身置为 `00000`，按原生字节扫描规则排除 model 值及 max_tokens、fallbacks、fallback_credit_token 字段；不是完整 JSON 的直接哈希，也不是认证签名。仅改写插件自己生成的计费块，不改动用户文本中的同名内容。
- 普通交互推理发送完整历史和 diagnostics，不启用 Message Threads，也不获取远端 GrowthBook 配置；没有宿主生产的灰度来源，客户端传入的 thread／diagnostics 不可信。`diagnostics.previous_message_id` 使用同作用域上一成功响应的上游消息 ID，无已知值时为 null；`cc_prev_req` 使用上一成功响应的 `request-id`。压缩请求和含原生压缩块的回放不发送 diagnostics；所有请求均不发送 thread 或 thread beta。缓存断点放在身份／最后一个客户端 system 块及消息末尾可缓存块，TTL 为 1h；tools 不加断点。beta 按最终模型、thinking、工具与实际字段条件选择，不伪造未知灰度分配或工具耗时。
- 第一方主请求的 `claude-code-20250219` beta 不因型号含 Haiku 而移除；per-turn effort 与 mid-conversation beta 按已知型号及显式发现能力选择，不照搬单一型号的整组头。无工具历史时继续发送官方允许的 `x-claude-code-sandy-plover: []`，不把 SDK 日志中的缺省显示当作真实 wire 缺失。
- 支持 effort 的型号统一以 `medium` 为插件默认值，而非逐型号复刻 CLI 的 high／xhigh 默认；显式有效强度、预算换算、目标控制与型号能力限制仍优先，不支持 effort 的型号不注入该字段。满足 context 编辑条件、思考未关闭且调用者未声明策略时，默认发送 `context_management.edits: [{"type":"clear_thinking_20251015","keep":"all"}]`；原生压缩不混入该默认编辑，显式 capability 禁用仍生效。公共 IR 目前仅承载原生 compact 编辑，不能完整表达 thinking/tool 清理策略及空 edits 覆盖；本次不扩大公共 IR 或 codec 契约。
- 私有状态按宿主 transport affinity、账号、地址与客户端 profile 隔离，保存 prompt ID、用户轮摘要与计数、上一成功响应的请求及消息 ID 和时间，不保存消息、签名或凭据，并保留同一根对象中的其它字段；最多 64 项、24 小时元数据保留。没有可信 affinity 时不保存作用域状态；压缩操作不使用或更新该状态。调用方仍须保存完整历史。插件不自行重发生成请求，普通错误、认证、限流或服务端故障按既有分类交宿主；通用续接恢复与 Route 重试仍由宿主决定，不为 Claude 原生线程增加重试路径。
- Responses 历史继承或客户端显式声明的 `truncation: "auto"` 与 `truncation: "disabled"` 按目标是否支持统一处理：支持时保留原值，不支持时省略该控制，不因无法表达拒绝转换。Anthropic 不支持该字段，插件不擅自裁剪输入，超出输入窗口仍按 [Anthropic 的默认输入溢出行为](https://platform.claude.com/docs/en/build-with-claude/context-windows#context-window-overflow-behavior)由上游报错；这不保证 `auto` 的自动截断意图仍获执行，也不保证其它内容或 continuation 历史转换无损。已安装的独立 `claude-code` 插件需重新构建并导入才能获得此修复，单独更新宿主不会替换插件中的 codec。
- 请求头包括 `Accept-Encoding: gzip, deflate, br, zstd`（宿主解码响应）与 `Connection: keep-alive`；工具 `strict` 按 Anthropic 原生 grammar 编译预算投影：工具总数超过 20，或工具与输出 schema 合计 optional properties 超过 24、`anyOf`/`oneOf` branches 超过 16 时，只移除工具上的 `strict: true` 注释，保留显式 `strict: false`、schema、required 与 nullable/null 语义，不删除约束或把 null 改为字符串。预算内保留原声明；降级不保证严格工具入参约束仍由上游强制执行，见 [官方 structured outputs 限制](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)。显式 `tool_result.is_error` 保留；成功结果不补 `is_error: false`。非 deferred 的自定义工具按实测补 `eager_input_streaming: true`。
- 已知自适应型号未指定 display 时采用交互式实测的 `updates`；显式摘要或隐藏意图保持优先。Haiku 5.5 支持 adaptive 与 low／medium／high／xhigh／max effort，不接受旧 Haiku 的 enabled + budget_tokens；disabled 只允许 high 及以下。Haiku 4.5 的手动预算路径不变，依据[官方 thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)与[effort](https://platform.claude.com/docs/en/build-with-claude/effort)文档及 Haiku 5.5 真实 200 请求。
- [Claude 4.6 及以后不支持 assistant prefill](https://platform.claude.com/docs/en/build-with-claude/working-with-messages#prefilling-claudes-response)。Claude Code 插件在发送前按型号规则省略连续的末尾 assistant 消息，思考开启时同样省略不被接受的 prefill；此前的 assistant 历史及已完成的工具调用、结果保持不变。不把助手内容改成 user，也不插入合成的继续提示。代价是末尾响应前缀不会送达该型号；旧型号在思考未开启时仍保留原生 prefill，未知型号不按版本猜测。修复由重新导入构建后的 `claude-code` 插件交付，不在宿主按供应商分支。
- 已知差异：不伪造 CLI 灰度 dispatch ID、工具耗时、cwd／用户名、自动权限评估或原生 UI 操作记录；没有来源证明的旧历史计数仍未知。Stainless 平台头固定为 Linux x64；网关解析 JSON 后不保留客户端对象内的键顺序，工具 schema 与工具入参的嵌套键按字典序发送。TLS 与 HTTP 使用当前交互样本对应的 `claude-code` shape；形态一致不等于运行同一 Bun 实现，也不等于逐字节复刻全部请求正文或证明所有账号、模型、ZDR 策略与灰度组合均可用。
- 模型发现读取 `/v1/models`；额度监控读取 `/api/oauth/usage`，呈现 5 小时、每周、按模型的每周窗口与额外用量。
- HTTP 429 同时明确报告代表额度窗口为 `five_hour` 或 `seven_day`、对应窗口 `rejected` 且使用率为有限值并达到 100%，以及额外用量 `rejected`、原因为 `org_level_disabled` 时，按 `QuotaExceeded` 交给宿主直接切换 Target；缺失、未知或互相不匹配的证据，以及普通请求限流，仍保留既有分类。该判定由共享 HTTP 错误解析执行，不改变状态码或 `Retry-After`。已安装的独立 `claude-code` 插件需导入重建的插件包才能获得此分类修复，单独更新宿主不会替换已安装插件。
- 以订阅 OAuth 令牌在 Claude Code 以外的客户端调用可能违反 Anthropic 使用条款并导致账号受限；上游对客户端形态的校验可能随 Claude Code 版本变化而失效。

### Antigravity CLI 接入

Antigravity 由专属 `antigravity` 插件提供，不内嵌、不复用普通 Gemini API-key 身份。运行 `task build:vendors:all` 后，本地导入 `target/vendor-plugins-all/manifest.json` 中对应的 Wasm；发布附件沿用 `stravia-vendor-antigravity-v{version}.wasm` 命名。

1. 添加 **Antigravity** 供应商，选择 OAuth。浏览器授权后，从 Google 网站复制授权码并粘贴到界面的秘密输入框；不监听 loopback 端口。如果网站回调没有显示授权码，但跳转地址已有 `code` 参数，只提交该参数解码后的授权码，不提交整个回调 URL。
2. 保存连接并同步账号模型。项目异步接入未完成时，保留凭据与 operation，后续真实发现请求检查一次进度并明确报告 pending；不猜 project/tier 或自动重试。
3. 将发现的模型绑定到 Route。额度页读取账号共享池、剩余比例和重置时间；缺失额度不显示为零，未知窗口保留，disabled 不自动等于 exhausted。

额度页为 Claude/GPT 与 Gemini 模型组的已知五小时、每周标签提供中英文显示，以及已知标签的 disabled 提示。翻译只在既有展示层识别明确标签，不检测 Vendor 身份、不改持久化 bucket/guard key；未知名称与窗口完整保留上游原文。

推理清单只采用 `agentModelSorts[].groups[].modelIds` 引用的 Agent 选项，不把完整 `models` 目录或搜索等其他用途清单当作可推理模型。解析真实 `deprecatedModelIds` 重定向并排除 disabled 后，按官方显示 slug 的家族与明确档位合并：例如 Gemini 3.1 Pro 登记为 `gemini-3.1-pro`，支持 `low`、`high`，默认实际 selector 为 `gemini-pro-agent`。同名或同 enum 不构成合并依据，Thinking 标签不自动生成离散档位。筛选和映射证据见 [模型映射调研](../research/antigravity-model-mapping.md)。

模型同步同时读取 `imageGenerationModelIds`，按真实图片 ID 独立登记，不按推理档位折叠，也不创建虚拟默认型号。图片专用条目仅声明 `media_image` 和图片输出；仅与相同 ID 的 Agent 条目合并时保留双输出。`supportsImages` 仍只表示图片输入。升级独立插件并重新同步后，可将这些模型绑定到图片 Route，再在媒体生成设置中选用；未固定的规格字段随同步刷新，固定字段保持不变。生图复用原 OAuth、project 与连接，按官方 agy 1.5.1 抓包走 `generateContent` 定长 JSON，不修改普通 Infer 白名单，不在请求时换成账号广告的另一型号。请求映射、单图与收存契约见[媒体生成](media-generation.md#antigravity-原生图片生成)；本地响应回放不代表所有真实账号都有生图权限或额度。

合并复用现有 Provider Model 的 `family`、`selector`、`reasoning_efforts` 和插件专有 extensions 契约，不新增前端供应商分支、公共 API 或数据库 schema。`antigravity` 选择表保留每档真实请求 ID、显示 slug 和规格；家族规格只承诺各档共有的输入模态与最小已知上下文，任一档上下文未知时不登记家族上下文。未指定档位时使用上游默认项所在档位，其他家族使用其 Agent 引用顺序中的首项；显式 Effort 必须匹配表中已登记档位，不存在时在网络请求前报错，不编造 ID 或回退到另一档。选择后的 envelope 模型 ID 与 thinking 控制均取对应档位，不另按通用 Effort 推导 Gemini budget；独立的原生 Budget 等控制保持既有行为。

推理未指定输出上限时发送 `generationConfig.maxOutputTokens: 65536`，显式上限保持优先。目录的数值 `thinkingLevel` 在最终 wire 投影为 CLI 枚举名：`0`–`6` 分别为 `UNSPECIFIED`、`LOW`、`MEDIUM`、`HIGH`、`MINIMAL`、`EXTRA_HIGH`、`MAX`；只改变表示，不改变已选档位。显式扩展档位不再被公开 Gemini codec 折叠为 HIGH；预算路径及 `includeThoughts` 语义不变，非法目录档位仍在发送前失败。

升级时重新导入构建后的独立插件，再同步模型。旧逐档模型记录与 Route 绑定不自动删除或改绑；未再发现的记录沿用现有 missing 与可用状态规则，已有配置不会自动迁移到家族模型。需要统一档位选择时，从清单添加家族模型并显式调整原绑定。

客户端标识使用 `antigravity/cli/<version>`，不能省略 `/cli/`：实际同账号差分验证中，省略会使上游不返回 Gemini 3.7／3.8 Flash 的 Agent 档位，而不是本地合并规则删除了这些模型。同步结果取决于真实账号响应，不固定承诺家族数量；此次修复不改账号、project、端点或权限。

OAuth 使用 CLI 1.2.16 静态核对的 `https://accounts.google.com/o/oauth2/auth`、`https://oauth2.googleapis.com/token`、固定网站回调 `https://antigravity.google/oauth-callback` 与 consumer 七个 scope。宿主通用契约允许 `AuthorizationCode + callback=None + manual_input.type=text`，有效模式为 `manual`、listener 为 `not_required`；缺少手动 text 输入的无 callback 授权码描述符仍拒绝。现有 WebUI 按描述符渲染，不增加供应商分支。

推理采用 daily Cloud Code 的 `streamGenerateContent?alt=sse`，解包 response 后复用 Gemini codec。请求头由插件构造 Bearer、JSON Content-Type 与 agy 1.5.1 Linux/amd64 的完整 CLI User-Agent（含 `cl=1014269460`）；通过宿主压缩策略协商 `Accept-Encoding: gzip` 并解压响应，不只手工添加声明。不转发下游头，不注入社区代理自选的 Client-Metadata/X-Goog-Api-Client，也不为 SSE 额外添加官方未发送的 Accept。编码前清除非 Google 扩展及未核实的 canonical 控制；工具定义、工具选择与响应 Schema 按 CLI private master 构造，避免公开 Gemini codec 丢失其已有约束。最终请求体经递归白名单投影；工具业务 JSON、`functionResponse.parts` 中的附件和真实签名保留，不制造签名或伪成功。工具结果与附件共用调用边界，跨协议转换、上下文重写和 Observation 不再只留下业务 JSON。完整字段、开源代码分析、CLI 二进制证据、验证边界及条款风险见 [Antigravity OAuth 调研](../research/antigravity-oauth.md)。

Schema 投影在 schema 节点将布尔 `true` 等价转换为 `{}`，将 `false` 等价转换为 `{"not": {}}`，因此 `{"not": true}` 仍表示禁止任意值，不通过删除约束绕过本地校验。`additionalProperties` 的原生布尔开关，以及 `default`、`example`、工具参数与结果中的业务布尔值保持原样。

已在隔离 Server 中通过默认 Edge 完成真实 Google OAuth，并用 `gemini-3.5-flash-lite` 验证四种客户端协议各三轮上下文与第三轮 SSE 终态；Responses 使用 `previous_response_id`，Gemini 保留真实签名。四协议还分别通过同步工具调用后流式回填、流式工具调用后同步回填：客户端执行声明的整数加法工具，并校验模型逐字返回工具生成的随机 receipt，确认工具结果实际进入下一轮。此验证不覆盖内置平台工具或 MCP。真实额度 API 与 Edge 额度页的四个共享窗口、剩余/已用切换和重置时间一致。这仅证明测试账号当时可用，不保证其它账号资格、全部模型或未来可用性。第三方 OAuth 使用可能违反 Google 条款并导致账号暂停或终止；私有协议与客户端识别也可能变化。固定 Linux/amd64 User-Agent 不代表复刻了原生 TLS、HTTP 或 JSON 序列化指纹。专属插件要求支持网站回调后手动授权码的 Stravia 版本，单独导入到旧宿主不能获得该流程；工具回放修复同样需要更新宿主与重建的独立插件。

### Provider 图标标识

- `ProviderDescriptor.icon_svg` 可选，携带编译进插件的自包含 SVG。单图不超过 512 KiB；宿主加载时校验大小和 SVG 根格式，不将该校验当作 SVG sanitizer。图标通过既有宿主图像接口和 WebUI `img` 显示，响应保留 CSP，不将插件 SVG 注入页面 DOM；无需修改 WIT 函数布局。图像接口属于公开静态资源，不要求管理登录；入口 origin 策略仍生效。
- 宿主优先返回插件内嵌 SVG，再尝试目录 logo 和网站 favicon。`base` 的全部 Provider Profile 均不声明 `icon_svg`，也不维护品牌 SVG 清单或图标别名；宿主按原生 `catalog_id` / `provider_id` 请求 Models 图标。`base` 插件自身的 Stravia 标志不属于供应商品牌图，继续保留。专属插件直接声明各自适用标志。Command Code、Devin、Antigravity 的第一方资源来源记录在各自 SVG 内。
- 更换图标需要重建并重新导入独立插件；仅更新宿主不会改写已安装组件。`base` 随宿主更新。品牌名称和图形归各自权利人所有，公开下载地址不等同于再分发授权。
- 访问远端 `/logos/{id}.svg` 时，图标 ID 优先使用插件声明的 `catalog_id`，没有该映射时使用 `provider_id`，不得使用已保存连接 UUID。例如 `openai-codex` 的 `catalog_id = openai`，只有未声明内嵌 SVG 时才请求 `/logos/openai.svg`。
- 按选定 ID 获取目录图标失败后才进入网站图标回退；已经选择 `catalog_id` 时，不再追加一次 `provider_id` 目录图标尝试。网站图标来源优先使用插件明确声明的官网地址；没有官网声明时，使用已保存连接 `base_url` 的 origin。不得把文档地址自动视为官网，也不得猜测域名。官网一旦被选为来源，获取失败后不再请求连接 origin 的 favicon。
- 插件可声明内嵌 SVG、图标身份与网站来源；只有缺少内嵌 SVG 时，宿主才统一获取目录 logo 与网站 favicon，保存在 Stravia 实例磁盘缓存中并供 UI 读取。Desktop 缓存在本机实例，独立 Server 缓存在服务器实例，二者都跨进程重启复用。浏览器不再直接请求外部 favicon，也不以浏览器缓存代替实例持久缓存。
- 网站图标回退仅请求选定网站 origin 的 `/favicon.ico`，不请求或解析首页 HTML 寻找其他图标；接受仅通过 HTML 声明图标而未提供 `/favicon.ico` 的网站无法获取图标。宿主下载遵守既有网络访问限制，且不携带 Provider 凭据。
- 目录 logo 与网站 favicon 统一使用 24 小时 TTL。有效缓存直接读取且不访问远端；缓存过期后在下次需要时按需更新，不新增定时刷新任务。每个已选来源更新失败且存在旧缓存时继续使用旧图标；没有缓存时才进入下一级回退。官网存在但获取失败不等同于没有官网声明。
- 选定网站 favicon 获取失败且没有缓存时结束网络尝试；`custom` 显示 Lucide `Plug`，其他 Profile 显示名称首字母。WebUI 不再打包或查找另一张品牌 SVG 表；调用方明确传入的 `icon_svg` 仍可直接显示。
- Models 缺少 `providers/<id>/logo.svg` 时会返回 HTTP 200 与通用火花图形，未知 ID 也遵循同一默认图规则。宿主按正常 SVG 缓存并显示，不将其视为请求失败，也不继续尝试 favicon 或首字母回退；真实品牌图应在 Models 源仓库补齐，不在 Stravia 添加映射或内嵌兜底。模型目录身份 `catalog_id` 同时控制模型关联，不能仅为换图标而改写。

## 已确认的供应商能力覆盖

- 一个供应商 Profile 可以统一提供推理适配、自定义上游编解码、OAuth、自定义模型发现、额度获取与供应商特有计算，以及自定义 Provider 选项声明和校验；拆为多个包不缩减任何既有供应商能力。
- `stravia-vendor-base` 承接各专属 Profile 之外的全部现有供应商接入，包括 Anthropic API-key 接入、云认证与云协议、模型发现、额度查询和供应商差异。它在单一 `base` Vendor 身份内按输入 `provider_id` 分派 Profile，不把 Profile 暴露为多个 Vendor。
- `stravia-vendor-base` 的目标边界是在运行时消费 `https://models.stravia.cn/providers.json`，仅把能够映射到 base 已支持协议与认证实现的目录条目注册为 `ProviderDescriptor`。远端新增的兼容供应商无需更新 Stravia 或重新构建 base 即可添加；目录中存在但协议或认证方式尚未受支持的条目不得注册为可用 Profile。专属 Profile 继续由各自 dedicated Vendor 整体接管，不与 base 合并。
- base 启动时优先使用本地最后一次成功供应商清单，没有缓存则使用插件内嵌供应商清单，随后尝试远端更新。首次离线仍可选择内嵌供应商，已有安装断网时仍可使用缓存供应商；内嵌清单只用于 bootstrap，不限制远端动态新增。缓存、内嵌和远端条目都必须按当前 base 已实现的协议与认证能力校验后才能注册。
- 供应商模型目录属于对应 Provider Profile，由 base 在运行时获取 `https://models.stravia.cn/providers/{provider_id}/models.json`；这里的 Provider 是供应商接入身份，不是已保存连接 UUID。内嵌供应商清单不包含全部 provider-scoped 模型数据。Core 保留 `https://models.stravia.cn/models.json` 的 Canonical Model 数据，只在供应商模型目录没有数据时作为回退来源。模型集合仅由供应商发现或管理员明确添加确定；Core 不通过回退增加成员或声明模型可用，只为其中缺失元数据的模型补充 Canonical Model 数据。
- 目录关联与缺省模型来源由已选 Profile 和 channel 决定，不由管理请求的 `catalog` / `custom` 创建分支决定。新建连接保存 Profile 的 `catalog_id`；模型来源优先使用显式配置，否则仅在 channel 声明消费目录且目录中存在对应供应商时使用 Catalog，其余沿用 channel 声明的默认来源。显式静态列表仍优先决定模型集合，账号级发现渠道不会被目录清单替换。
- 已保存连接未记录目录关联或模型来源时，发现与同步使用本次执行锁定的 Profile 声明解析缺省值，无需改写连接。同步更新未固定的规格字段及最后有效来源快照，保留人工固定字段；恢复同步只使用此来源快照，不重新读取目录。`reasoning_efforts` 因同步、编辑或恢复而改变时，同一事务重算 Generated Mapping，已有 Target 的手改映射不受影响。
- 模型集合已经由上游发现或管理员添加，但供应商模型目录元数据获取超时、返回 HTTP 500 等失败时，允许用 Core Canonical Model 补充元数据。必须明确报告目录获取失败，不得把本次目录刷新标为成功，不得覆盖已有有效数据，也不得扩大模型集合。真正的上游账号级模型发现失败不适用该回退，不能用全局目录冒充成功。
- base 的供应商目录与 Core 的全局 Canonical Model 目录独立刷新，不再跨模块等待或原子切换到同一 revision。base 新增兼容供应商不受 Core 下载失败阻塞，Core 刷新也不等待 base；接受两边短时 revision 不同。两边分别校验各自下载的一致性，失败不得标记成功，并保留已有有效数据。
- 除上述已确认变更外，目录迁移遵循 [ADR-0073 定义的插件化迁移前原生基线](../adr/0073-register-runtime-catalog-profiles-through-base-vendor.md#迁移基线)，不得从插件化后的当前实现或 `ProviderDescriptor` 反推继承行为。继承的刷新节奏是启动后立即后台刷新、之后每小时刷新；失败明确记录并保留当前有效快照。
- 成功刷新整体替换供应商目录。远端删除供应商后，不得再从该 Catalog 条目新建 Provider；已经保存的 Provider 记录不自动删除，普通推理不只因目录删项停用。已有 Provider 使用 Catalog 同步模型时，缺项仍明确报告 `ProviderNotFound`，不得冒充空 scope 成功；上文确认的 Core 元数据回退只在模型集合已由上游发现或管理员添加确定时补充元数据，不恢复被删条目的新建资格。
- 元数据富化继承原生顺序：先精确匹配 provider scope 的 upstream model ID，再按完整 canonical ID 精确匹配，随后按最右段 ASCII 小写键做唯一匹配；多个候选不匹配，最后才使用 bare metadata。该顺序不扩大模型集合。动态新增 Profile 的持续注册方式、持久化 schema 与缓存目录属于实现设计，本轮不另设产品策略。
- base 通过 `sync-catalog` 导出实现该所有权：宿主把 catalog base URL、上次成功快照与可选的 scope 供应商 ID 传给 guest；guest 取回 `providers.json` 与 `providers/{id}/models.json`，返回完整 Profile 集与原始 body，由宿主持久化为 last-good。公布的 revision 与宿主快照一致时，guest 复用该快照、不重复下载 `providers.json`，也不返回新的 body，宿主因此不重新解析与落盘。base 的构建脚本仍从 Core 的 `assets/providers.stravia.json` 生成内嵌 bootstrap Profile 表——该表只服务首次离线启动，不替代远端清单。目录删项不进入删除路径：被删 Profile 保留为 retired 状态，存量连接继续准入执行，新建入口与展示列表不再提供。ADR-0018 的跨模块原子 revision 与 scoped 失败约束仅按 ADR-0073 修订，其他缓存和模型快照契约保留。
- OpenAI-compatible（包括 embeddings）、Anthropic、Gemini、Open Responses 四类标准 codec 由共享库提供，作为可复用实现源码由各 guest crate 按需引用。
- DeepSeek Profile 位于 `base`，复用标准 codec 并保留必要的供应商差异与额度能力，不要求为复用标准协议单独维护一套 codec。
- Codex、Grok、Command Code、Devin 使用独立专属插件。Devin 保留自定义 Connect-RPC / protobuf 编解码、OAuth、模型列表与相关元数据、AssignModel 辅助调用及额度获取；Command Code 的自定义 Provider 选项注册属于插件契约，不能依赖宿主的供应商专属前端或后端分支。
- 上述范围覆盖现有能力，不表示每个 Profile 都必须实现每种供应商功能；具体能力由对应 `ProviderDescriptor` 声明并按 Profile 独立校验，不能从同包其他 Profile 合并推断。

### 多能力扩展的已确认范围

- 同一个 Vendor Plugin 可以提供模型推理、完整联网搜索、媒体生成中的任意非空组合；允许只提供完整搜索或媒体生成，不要求伪装成可对话模型。同一 Provider 连接可以供其支持的多种能力复用。
- 本次联网扩展限于 Codex hosted search 一类完整 Web Search 后端，不包含 Local Web Search 使用的基础 search/fetch Provider；完整研究报告与基础检索结果不合并为一种契约。
- 媒体生成沿用现有设计的图片生成与参考图编辑范围，不因插件能力可组合而提前增加音频或视频类型。
- 媒体生成沿用 Provider Model → Target → Route，不另建直接绑定 Provider 的执行路径。Provider Model 可表示纯图片模型；Codex 的 GPT 模型可以同时支持推理与图片生成。
- 媒体生成绑定及执行按所需能力校验；纯图片模型不能被普通聊天请求选中，不以虚构聊天能力满足路由接入要求。继续遵守媒体生成设计中所有已启用 Target 的资格校验规则。
- 完整联网搜索引入 Route 绑定，替代 Codex 专属的固定 Provider 与 upstream Model 绑定。搜索与其他能力一致，Target 必须绑定 Provider 和非空白的上游模型，并通过对应 Provider Model 的能力校验。
- Channel 描述符不再提供搜索模型可省略的开关。第三方 Rust 描述符构造代码需要同步移除该字段；该描述符字段删除本身不改变 WIT 布局或 canonical payload 版本。通用 `ProviderSnapshot.model` 仍可用于不涉及 Target 的 Provider 操作，但宿主执行搜索 Target 时始终传入所选模型；旧组件 JSON 中的额外字段不会恢复无模型执行权限。
- 独立搜索服务以 [Parallel Responses API](https://docs.parallel.ai/responses-api/responses-quickstart) 一类直接交付带引用综合答案的服务为参考，仍接入完整 Search Report 契约，不扩展为仅返回基础检索结果的来源。正式接入必须提供可选择的真实 Provider Model，不自动列入本期交付清单。
- 外部完整搜索后端按单次独立调用处理，不提供续接，不跨请求保存可用于续接的上游研究会话；即使供应商具有会话能力，本次也不使用。此决定不取消既有 Local 搜索的续接能力。
- 外部搜索沿用 Route 的选择、重试与 Target 切换策略，由插件提供上游错误分类，宿主决定执行。明确可重试的上游错误可在结果提交前重试或切换；用户取消、参数错误及权限错误不触发。接受可能重复执行与重复消耗额度，不承诺恰好执行一次。
- 更新允许移除已被使用的能力，必须明确展示受影响绑定。保留原绑定并标记能力不可用，执行时明确失败，不自动换账号、改绑或降级为普通推理；数据兼容时已开始的调用仍由旧版本完成。
- 内嵌 `base` 的自动更新即使移除正在使用的能力，也继续更新，不新增暂停确认步骤；更新后展示受影响绑定及能力不可用状态。按格式版本差异重置不兼容数据也自动执行，但不得跳过宿主契约兼容性检查。
- 宿主保留公开工具、平台能力开关、报告及引用校验、Artifact 收存与归属、历史和用量；插件提供类型明确的供应商能力，不注册新的公开搜索或生成工具。Codex 同连接的推理、hosted search 和托管图片生成纳入统一插件验收。
- 本轮多能力扩展已整体确认，实施与验收以本文及对应能力设计中的契约为准。

## 已确认的统一运行机制

- 最终所有模型 Vendor 均通过同一套 Wasm 插件机制加载与执行，不长期保留原生 Vendor 通道或供应商专属宿主执行分支。
- 程序默认只内嵌 `base` 自包含 Wasm 包；全新 Gateway 因而只预装 `base`。`openai-codex`、`xai-grok`、`command-code`、`devin`、`claude-code` 等专属包不内嵌，必须从本地分发包手动导入后才可用。
- 缩减默认内嵌集合不会自动删除实例中已经安装的专属插件及其数据；但不再内嵌的专属插件没有程序随附版本可供恢复或随宿主自动升级，后续更新仍通过本地包导入。
- 宿主仍拥有通用网络、存储、授权、调度及客户端协议处理，不将整个网关移入 Wasm；供应商拆分不改变这些平台职责。
- 内嵌与本地安装是分发来源的差异，不构成两套供应商能力契约，见 [ADR-0070](../adr/0070-run-all-model-vendors-as-wasm-plugins.md)。

## 已确认的自定义协议范围

- Vendor Plugin 的自定义协议仅用于上游接入，不开放客户端入口注册。
- 插件可以自定义上游请求、响应及流式编解码，包括 Devin Connect-RPC / protobuf，但必须接收与产出宿主规定的 canonical 语义。
- 私有上游身份通过开放的 `ProtocolIdentity` 精确保留，不进入共享 `Protocol` 枚举或 `ProtocolEndpoint` 常量、解析与别名表。Devin 的 `devin-connect` 和 Command Code 的 `command-code/generate/v1` 由所属插件返回；私有请求编码器和流解析器不实现依赖共享标准 endpoint 的 `ProtocolAdapter`。
- Command Code 的 NDJSON 信封不提供原生响应 ID；其 codec 在首个有效事件前产出一次 canonical `MessageStart`，由宿主绑定 Generation Chain 身份。Responses 客户端收到的 `response.id` 与保存的续接节点一致，流式响应后的 `previous_response_id` 可回放原始思考与工具结果。
- Devin 的 indexed 思考项可在 Connect EndStream 才封口，晚于同一响应的正文和工具调用。宿主保留该思考项开始时的正文前/后位置，不以封口时的位置改写 Marker 载体；实时交付与最终历史投影使用同一个 Marker，继续校验顺序并保留原始思考。
- 插件不得注册任意 HTTP 路由、客户端认证方式或 MCP 工具；安装供应商插件不等于对外提供该供应商的原生服务入口。
- 宿主决定对外开放的客户端协议，仍可引用共享标准 codec 库；增加新的客户端协议需要宿主支持，不通过 Vendor Plugin 安装隐式开放。
- 目标无法表示的任务语义、控制与扩展默认省略，保留可表达部分；请求、响应、stream delta 与 continuation 历史均适用，不提供 strict toggle，也不允许绕过 canonical 契约直接透传客户端流量。该规则可能损失图片、工具及关联结果、硬约束、多候选与历史，不宣称无损。Decoder 仍拒绝非法源 wire 和解析错误；安全、凭据与网络错误不因省略政策而吞掉。不可表达的工具参数不能填成 `{}` 或 stringify 冒充支持；省略 delta 不制造失败终态，保留同 delta 可表达文本与真实 usage，不合成零 usage。
- 扩展与状态按目标是否能承载判断，不只按客户端入口是否相同判断。宿主在解码后加入的目标原生扩展必须保留，例如经完整父节点、目标与有效历史证明校验后的 Responses `previous_response_id`，以及 Responses 可承载的原生 compaction 状态；换到不能承载的目标时仍省略，不放宽续接证明或安全检查。
- 私有 wire 的表达能力由插件本地判断，不给共享标准转换器增加 Devin 或 Command Code 能力分支。Devin 原生 protected/redacted 思考载体必须保留用于回放；文档中的可表达文本和图片逐项转换。Command Code 仅使用已有原生载体：保留用户图片、文本、工具参数与结果及 reasoning/effort；不能承载的媒体子项省略，不杜撰工具媒体格式。
- 未解析为共享标准 endpoint 的身份（包括空 hint）收到 canonical inline 媒体字节，不因启用外部签名下载而改收私有 wire 未必支持的 URL。已知标准 endpoint 继续遵循原有媒体载体规则；Artifact 归属、保留期和下载授权不变。私有思考来源不能匹配客户端标准 endpoint 时，宿主使用通用 History Marker 保留原始条目及位置，不因身份未知而跳过回放保护。

### 客户端请求头读取声明

Channel 的 `client_header_names` 声明其需要读取的额外客户端头名。名称必须是小写、合法且精确匹配的 HTTP token，不允许通配符；未声明或集合为空时，不提供额外头。宿主始终按本次已选中的 Provider/channel 筛选，不把一个 channel 的声明授予其他目标，公共标准头的既有白名单保持。

描述符加载和执行时均拒绝凭据及 hop-by-hop 头的声明，包括 `Authorization`、`Proxy-Authorization`、`Cookie`、API-key 头、`Connection`、`Host` 和 framing 头；声明不是获取 Stravia 平台凭据的授权。值必须符合 HTTP 头字节规则：允许 HTAB 和 obs-text，拒绝其他 ASCII 控制字节及 DEL。插件决定如何使用已获准的上下文，宿主不维护供应商私有头名。

预置 channel 均不声明额外客户端头。Codex 的会话、逻辑轮和窗口只取宿主提供的作用域身份与历史证明，客户端自报的 thread、window、parent、turn 不得覆盖它们；描述父身份不授予父响应访问或续接权限。installation ID 取宿主持久安装身份，不再按凭据单独生成，按官方 `0.167.0` 只写入 `client_metadata.x-codex-installation-id` 与 turn metadata，不作为请求头发送。`x-codex-turn-state` 只回传对应 turn 内上游 HTTP 响应头给出的值。客户端的 `x-codex-beta-features`、`x-openai-subagent` 与 `x-codex-turn-metadata` 不读取或转发；插件自行发送官方固定 beta 与由可信事实生成的 turn metadata。`client_header_names` 仍是 SDK 描述 JSON 的可选字段，request format 仍为 1；持久状态迁移见 State 1.x 契约。独立 Codex 插件须重新构建并导入。

所有客户端入口的已知请求控制与会话 header 先归一到唯一 canonical 参数；目标 codec/插件从该参数按正向白名单投影，不从旧协议扩展、raw snapshot 或别名恢复一份冲突值。OAuth／订阅插件的参数清单以对应官方工具实际产出为准，不以公共 API 字段超集代替；未列入的参数与参数型嵌套成员透明丢弃，不转发、不使请求失败。平台不提供任意参数袋或 extra-body 注入。官方声明为任意业务映射的 metadata 与工具 schema／入参内部键不按参数清单递归裁剪；CLI 身份 metadata 等结构化控制的成员仍受白名单约束。缺失与显式 false/零/空值保持语义区别；显式 null 只记为统一 `resets`，出口不发送 null。统一字段由 SDK 的 format-1 桥接同时投影为迁移前的旧拼写，已安装插件无需重建即可读取能表达的控制；已删除的供应商专属控制不再提供给任何插件。旧独立 Wasm 不会因宿主升级获得新 codec，需要重建并导入。

Codex 的工具声明也受官方 [`ToolSpec`](https://github.com/openai/codex/blob/main/codex-rs/tools/src/tool_spec.rs) 与 [function／namespace／custom 类型](https://github.com/openai/codex/blob/main/codex-rs/tools/src/responses_api.rs)约束：按工具类型过滤外围控制，不把整个 tools 数组当作任意业务 JSON。保留 function／custom 的 defer_loading、namespace 中的两类合法声明、client tool_search 的 execution 与完整参数 schema，以及由统一 `NativeTool::WebSearch` 生成的 web_search 官方字段（域名过滤、用户位置与上下文大小）。工具 schema 与 grammar definition 的业务内容不裁剪；公共 API 的其他 hosted tools 不因同属 Responses 就获得 CLI 推理出站权限。CLI 推理 tool_choice 使用官方字符串形态，不透传公共 API 的对象选择；已有 Search／MediaImage 操作仅使用各自固定构造器及操作白名单，不能由客户端参数绕入。

Codex 推理的 `include` 同样遵循[官方客户端构造器](https://github.com/openai/codex/blob/main/codex-rs/core/src/client.rs)的能力边界，只保留统一推理控制派生的 `reasoning.encrypted_content`，不请求公共 Responses 的 `message.output_text.logprobs`。客户端的 `logprobs`／`top_logprobs`（包括 `top_logprobs: 0`）在 Codex HTTP 与 WebSocket 出站时省略，不通过关闭推理或重试绕过上游不支持的能力。标准 API codec 的 logprobs 投影不变；已有安装须重新构建并导入 `openai-codex` Wasm 才能获得修复。

函数工具只保留 `strict` 与 `defer_loading` 两个外围控制；`eager_input_streaming`、`allowed_callers`、`input_examples`、`citations` 与工具级 `cache_control` 不进入 canonical，官方工具确需的形态由所属插件按官方规则生成。改变工具声明或原生工具集合会改变有效请求身份，不能误复用旧 continuation。schema、grammar 与原生工具描述中的业务内容参与秘密保护，不因外围参数白名单而丢失。

Claude Code 的函数工具白名单依据 [2.1.317 官方发布包](https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-2.1.317.tgz)与真实交互式请求核对：名称、描述、schema、strict 与 defer_loading 来自统一 `ToolSpec`，缓存锚点由插件按官方规则生成；非 deferred 自定义工具的 `eager_input_streaming: true` 由插件产生，不接受客户端覆盖。其 hosted 工具只允许由 `NativeTool::WebSearch` 生成的 `web_search_20250305` 及对应官方字段，不因公共 Messages API 支持其他工具就自动放行。升级 CLI 契约时应重新核对产出路径，不能通过客户端版本字符串覆盖来扩大白名单。

### Custom Provider Profile 与协议选择

- `stravia-vendor-base` 只注册一个 `custom` Provider Profile。管理员进入 Custom 后，从 base 明确声明支持的上游协议中选择；当前可选范围包括已经实现的 OpenAI-compatible、Open Responses、Anthropic Messages 与 Gemini。专属 Vendor 的私有协议不自动加入 Custom。
- 管理面完全从插件声明生成协议选项，不维护硬编码清单，也不接受任意协议字符串。该 Profile 及其全部选择由 base 单独拥有，不从其他插件合并声明，见 [ADR-0074](../adr/0074-select-custom-protocols-through-one-base-profile.md)。
- 现有 `protocol-openai-chat-completions`、`protocol-open-responses`、`protocol-anthropic-messages`、`protocol-gemini` 四个独立 Profile 入口并入 Custom。迁移已有连接时保留连接 UUID、凭据与 Route；具体选择标识、WIT 版本及迁移 SQL 是满足该迁移约束的实现选择，本轮不另设产品策略。
- 该合并已实现：base 只注册一个 `custom` Profile，其 channel 通过 `protocols` 枚举声明可选协议；管理写入按该枚举校验，不接受任意协议字符串。存量 `protocol-*` 连接由迁移 `0059` 改写为 `custom`，保留 UUID、凭据、protocol 值与 Route。

### Custom Responses 的传输与续接

选择 Open Responses 时，连接的「高级设置」提供两个独立的布尔选项，仍由插件描述符生成表单并通过 `vendor_options` 保存：

|选项|默认值|契约|
|---|---|---|
|`use_websocket`|`false`|为普通推理尝试上游 Responses WebSocket；关闭时使用 HTTP/SSE。由同一 Responses URL 将 `http` / `https` 转为 `ws` / `wss`，不另设地址。独立压缩仍走 HTTP。|
|`use_previous_response_id`|`true`|尝试安全的上游 ID 续接；关闭时不再尝试该优化，始终发送完整有效历史，不关闭本地历史、客户端 Stravia `previous_response_id`、受保护推理恢复或 prompt cache。|

WebSocket 开启但 ID 续接关闭时仍复用合格的连接，只是不发送上游 `previous_response_id`。ID 续接开启不意味着一定发送 ID：请求控制、完整历史前缀、账号与 Target 必须符合既有等价性校验。Custom WebSocket 对所有续接 ID 都要求原子取得该 ID 正好是当前 tip 的可用连接；即使请求允许 `store=true`，也不能据此假设代理上游保存了响应。断线、换凭据、tip 前移或分支不匹配时发送完整历史，不把临时 ID 带到新连接。HTTP 继续遵守上游实际存储事实。

这些选项不修改客户端的 `store` 意图，也不控制角色兼容。所有 Responses 出站由共享 codec 将 system message 原位编码为 developer；HTTP/SSE、WebSocket、Custom 与 Codex 使用同一规则，无需额外开关。消息内容、顺序与独立的 `instructions` 保持，本地历史不改写。上游仍须真正支持对应传输及请求形态；WS 握手成功不表示模型请求已被接受。HTTP 回退和完整历史恢复只在宿主现有安全边界与预算内发生，不在结果已开始后重发。

CC Switch 4.0.25 的 Responses → Chat Completions 转换会丢弃 `previous_response_id` 和 `store:false`，不会替客户端补回被省略的历史。该入口必须发送完整 `messages` 历史，或改用 Responses；Stravia 不从缓存键猜测缺失的父会话。Stravia 的 Responses 出站 `store` 现默认 `false`，因此省略该字段也不会请求上游持久化，但这不补回丢失的父 ID。实测 Sub2API 0.2.18 的 Codex OAuth WebSocket context pool 会拒绝输入中的 system 消息；共享 codec 的统一 developer 出站投影覆盖这一消息形态，不把 system 提升到 instructions，也不改变上述续接约束。

### Custom 的思考字段映射与补齐

base 插件为 `custom` Profile 的 OpenAI-compatible Chat Completions 声明固定思考字段配置。管理员在 Custom 连接的「连接设置 → 高级设置」中选择已支持的上游字段，并按需开启缺失补齐；字段、中英文说明、协议显示条件及分组均由插件描述符提供，宿主复用动态表单，不添加 Core 或 WebUI 的供应商专属逻辑。其他协议隐藏这些选项，不提供任意字段名输入。

|选项|默认值|契约|
|---|---|---|
|`reasoning_field`|`"protocol_default"`|可选协议默认、`"reasoning_content"` 或 `"reasoning"`；显式选择同时控制上游响应读取和 assistant 历史回放的字段。未知旧选项不授予自定义字段转发权限。|
|`ensure_reasoning_field`|`false`|独立控制是否把 assistant 历史中缺失或为 `null` 的所选字段补为 `""`；不要求开启补齐才能映射真实思考。|

创建或更新 Provider 时，通过既有 `vendor_options` 保存配置。例如，上游使用已支持的 `reasoning`，且要求每条 assistant 历史都包含字符串字段：

```json
{
  "vendor_options": {
    "reasoning_field": "reasoning",
    "ensure_reasoning_field": true
  }
}
```

- 仅作用于 Custom 的 OpenAI-compatible Chat Completions。显式选择的字段是上游 `message` 和流式 `delta` 中的字符串属性；缺失、`null`、空字符串保持区别，非字符串且非 `null` 的值显式报错，不回退读取其他思考别名。协议默认沿用标准 codec 的读取行为，补齐时使用 `reasoning_content`。
- 上游响应读入 canonical 思考后，客户端仍按自身协议接收响应；Chat 客户端使用标准 `reasoning_content`，不需要识别上游私有字段。后续 assistant 历史按所选字段回传上游，包括带工具调用的消息；原有真实思考、空白字符和空字符串不被改写，其他角色及工具调用关联保持不变。
- 补齐关闭时，字段映射仍然生效，但缺失与 `null` 不被改写。补齐仅发生在上游请求体中，不回写权威历史、不生成虚假思考，也不改变推理强度或开启思考；它用于适配 IFM 等要求每条 assistant 历史都有字符串字段的模板，不能恢复已经丢失的真实推理内容。相关区别见 [reasoning context 调研](../research/reasoning-context-replay.md)。
- Open Responses、Anthropic Messages 与 Gemini 继续使用各自原生的思考结构；这些选项不会改写它们的上游请求、响应读取或历史回放，也不对这些协议执行思考字段的业务校验。新建草稿切换协议时保留已填写的 Chat 选项，切回后恢复显示。

## 已确认的技术栈与插件契约

- 唯一目标技术栈为 Wasmtime + WebAssembly Component Model + WIT，不同时提供 Extism 或另一套自定义 Core Wasm ABI。
- Wasmtime 负责加载、执行与能力隔离；当前 WIT 为 `stravia:vendor@1.0.0`、独立的 `stravia:net@1.1.0` 与 `stravia:state@1.1.0`。旧 0.x 独立组件须使用新 SDK 重建并重新导入，没有旧网络 ABI adapter；已发布的 1.0 Component 保持兼容。canonical format 与模型客户端协议不变；持久用户轮、原子状态与 journal 使用配套的 SQLite／PostgreSQL 迁移。
- `VendorDescriptor` 包含 `vendor_id`、版本、展示元数据、canonical 格式版本、`kind` 与 `providers`；`kind` 为 `fallback` 或 `dedicated`。每个 `ProviderDescriptor` 独立声明 `provider_id`、可选 `catalog_id`、channel、能力、配置、网络权限和数据兼容信息。
- `fallback` 描述符的 Vendor ID 必须是 `base`，可声明多个互不重复的 Profile；`dedicated` 描述符必须恰有一个 Profile，且 `provider_id` 等于 `vendor_id`。旧描述符形状不保留 alias 或兼容 shim。
- `ProviderSnapshot.provider_id` 在 SDK 与 WIT 中均为必填供应商 Profile ID，不是连接 UUID。运行时按该字段选择唯一 `ProviderDescriptor` 后再做能力、channel 和网络准入，不得合并其他 Profile；`base` guest 据此分派，专属 guest 拒绝其他 ID。
- channel 可通过 `default_models_source: "catalog"` 声明未指定来源时默认使用模型目录；未声明时保留插件自身的发现行为，宿主不以默认值覆盖已保存的来源。该声明只接受目录枚举值，不接受任意 URL，不表示其他 channel 不能选择目录。channel 另以 `consumes_catalog_models` 声明是否消费宿主注入的目录模型 scope；只有声明消费的 channel 才允许把目录作为默认来源，目录创建入口也只为这类 channel 保存 `catalog` 来源标记。宿主不按供应商 ID 猜测默认发现策略。
- `base` 保留既有清单优先级：显式静态模型优先；原生 OpenAI、Anthropic、Google、Ollama、OpenRouter、xAI 的账户发现，以及 Vertex 的渠道精选清单，不被历史 `catalog` 来源标记覆盖。目录别名仍使用自己的目录范围，不能继承另一供应商的账户清单策略。该判定属于 guest，不移回 Core；`base` 的描述符按同一 provider 判定集合输出 `consumes_catalog_models`，专属插件一律声明不消费。
- 发现操作与同步富化按 `catalog_id` 提供原始目录 scope，不再次套用目录的旧 channel 定义，且只在 channel 声明消费时才解析该 scope。只有 `ProviderNotFound` 表示可选目录条目不存在，不提供目录模型快照，由插件决定其发现行为；消费目录来源的插件必须对缺失快照报错，不能返回伪造的空成功。目录访问或解析失败仍向上传播，不触发隐藏回退。
- 已保存的非 `catalog` 发现地址保留完整路径与查询参数，并使用所属供应商的发现认证策略；不能重新拼成推理基址的 `/models`，也不能把所有显式地址一律改为 Bearer。Google 官方原生目录使用 API-key 查询参数，自定义目录保留既有 Bearer 约定；标准 Anthropic 目录保留 `x-api-key` 与版本头。
- `descriptor`、`select-protocol`、`execute` 的导出结构保持不变。首先提供 Rust 插件 SDK，以复用现有供应商实现；WIT 契约不限定插件必须使用 Rust，其他语言的实际工具链兼容性需要验证。
- 发布产物为单个自包含 Wasm Component，携带锁定的 codec 依赖；不直接跨契约暴露 Gateway、数据库对象或 Rust trait 内存布局。
- 宿主与插件通过明确版本的类型和资源交互。Wasmtime 版本以根目录 `Cargo.toml` 与 `Cargo.lock` 为准，guest 工具链以 `rust-toolchain.toml` 为准；技术栈决策见 [ADR-0072](../adr/0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)。
- SDK 的 WASI WIT 依赖统一为 `0.2.12`，来自 [Wasmtime v49.0.1 的 WASIp2 定义](https://github.com/bytecodealliance/wasmtime/tree/v49.0.1/crates/wasi/src/p2/wit/deps)。宿主接受稳定版 WASI `0.2.x` 的既定受限接口，包括 `0.2.9`、`0.2.12` 及同一 Component 内不同 patch 的混合导入。WASI patch 兼容不放宽 Vendor、Net 与 State 的独立接口准入：Vendor 支持稳定 `1.0.x`；Net 与 State 的已知接口支持至 `1.1.x`，新增接口不能反向声明在较早 minor 下。其他主版本、预发布版与未知接口拒绝，同主版本但更高 minor 返回需要升级宿主。build metadata 不影响判断，函数、类型和资源仍必须通过 Linker 校验。
- WASI 准入按完整接口名称与稳定版本系列校验：仅允许既有的 stdio、terminal、空 environment、exit、clocks、random 与 io 支持接口；拒绝相似名称、无版本、预发布版及其他 major/minor 系列。SemVer build metadata 不影响稳定版判定。版本系列匹配不承诺未来所有功能可用：[Wasmtime Linker](https://docs.rs/wasmtime/49.0.1/wasmtime/component/struct.Linker.html) 仍校验所需函数签名和资源类型，宿主缺少函数或类型不兼容时明确加载失败。`WasiCtx` 不继承宿主 stdio、环境或目录；filesystem、sockets 与 WASI HTTP imports 始终拒绝，插件网络仍只能使用受控的 `stravia:net` 接口。
- 当前 Rust 预编译标准库仍导入 WASI `0.2.9`，因此新构建的插件可以同时包含 SDK 的 `0.2.12` 与标准库的 `0.2.9` imports。更新项目 `wasip2` 依赖不会替换工具链的标准库；本次不更换 Rust 工具链或自建标准库。构建产物的实际 imports 可用 `wasm-tools component wit <component.wasm>` 检查。
- 推理与原生压缩在执行前调用同一固定版本的 `select-protocol` 导出，由插件根据连接快照与 canonical 请求选择实际上游协议。该阶段只允许纯计算，禁止网络、私有状态和事件副作用，并受相同的取消与截止时间约束。宿主随后基于返回协议确定回放身份和语义转换，不维护 OpenAI 等供应商的协议偏好分支。

Rust 运行时执行入口接收 SDK 的操作类型与已编码输入，准入直接检查连接快照与 channel，不将完整 canonical payload 再解码为第二份 Native IR。请求准备、模型覆盖与续接使用共享 `Arc<AiRequest>`，只在真实修改时分离；文本叶子 `MessageContent::Text` 与 `ContentBlock::Text.text` 使用 `Arc<String>`。Rust 调用方从 `String` 构造时使用 `.into()`，从字符串字面量构造时使用 `.to_owned().into()`，编辑共享文本时使用 `Arc::make_mut` 或既有的 `output_text_mut()`。这些共享所有权调整只改变 Rust 源码接口，本身不改变 canonical JSON、格式版本或 WIT 布局；当前 Vendor ABI 切换的重建要求仍按本节执行。

`ProtocolAdapter::encode_request`、`ProtocolPair::encode_request`、`ProtocolTransform::encode_request_with` 和 `vendor_common::common::encode_inference_request` 消费 `AiRequest`，不再提供借用请求的编码入口。标准协议经共享准备入口按目标能力省略不可表达部分，私有 wire 由所属插件准备，再消费此次编码的 canonical 视图，不损坏共享历史快照；真实编码、解析与安全错误仍返回，目标表达能力不足不再整体拒绝。调用方先保留消费后仍需要的 model、stream、display 或 compaction 字段；确实需要继续使用完整请求时才显式克隆。四类标准 codec 在文本唯一归属时取出底层 `String`，共享文本仍保持只读；已拥有的原生字段、工具定义、工具结果和数组/对象直接移入 wire JSON，避免父对象再次序列化整段正文。已安装插件的性能优化需要重建并重新导入；内嵌 `base` 随宿主构建更新。

SDK 私有 canonical 编码与 base provider 请求体编码对不可变快照先计数，再按序列化长度分配 JSON 缓冲区，避免大文本触发容量翻倍；公开 generic payload 编码仍保持单次序列化，不额外调用外部实现的 `Serialize`。Wasmtime 继续启用 fuel 与操作取消/deadline，异步让出间隔为每 1,000,000 fuel；不减少总执行预算、不关闭沙箱，也不改用 Native 执行。

Canonical `MessageContent` 与 Chat 文本/parts 解码直接接收 owned string 或 typed sequence，不经 untagged 缓冲的借用分支复制整段正文；wire 字节的文本拥有成本仍保留。字符串/数组形状、原生工具 JSON、任意精度 wire 数字与非法内容拒绝契约不变；JSON backend 仍为 `serde_json`。

`execute` 的宿主绑定使用 owned lowering：参数复制进 guest 线性内存后，宿主不再为整个上游等待保留 canonical JSON 输入缓冲。宿主调度通过 Wasmtime concurrent call API 驱动，但一次调用仍使用独立 Store、只执行一个操作，不并发复用实例或 HostServices。`async | store` 是宿主生成绑定选项，不改变同步 WIT 导出；guest 的 component async、附加 async builtins、stackful async 与 threading 扩展显式禁用。取消、deadline、数据代际校验、受控网络与资源析构边界保持不变。

### State 1.x 与原生请求状态

`stravia:state` 独立于 Vendor 和 Net 版本化。1.0 的 `store` 提供操作范围内的 cell；1.1 只新增 `journal` 与 `volatile` 接口，不修改旧 `scope`、snapshot 或错误结构。冻结快照与摘要位于 SDK 的 `wit/released/`；修改当前契约后必须运行兼容检查并实际加载旧 Component，不能重新生成旧快照来消除不兼容报告。

- `provider` cell 归属于获准的 Vendor／Provider；`conversation` cell 还归属于 Principal 与本次获准的会话 lineage。guest 只提交 key，不选择连接、主体或分支 ID。纯协议选择、目录同步和没有对应会话准入的调用不能借此访问会话状态。
- key 最多 128 UTF-8 字节，每个 scope 最多 16 个 key，单个 payload 最多 1 MiB（1,048,576 字节），包含 provider、conversation、volatile 与 journal 的 cell payload；journal 正文的 64 MiB 配额不变。SDK、宿主和各存储实现共用 `state::MAX_PAYLOAD_BYTES`，超限不改变原值、revision 或 journal head。`load` 同时返回 revision 与可选 payload；从未存在的 key 为 revision 0，删除生成非零 revision 的 tombstone，不能用删除重置 revision 绕过 ABA 防护。CAS 只有版本冲突返回 false；取消、存储错误和配额耗尽均显式返回错误。冲突后只能重新读取并计算，不能重发外部请求。放宽容量不改变 WIT 布局；冻结的发布快照仍保留当时的容量说明。
- State 错误码是开放字符串：非法 key 或跨操作 journal handle 为 `invalid`，没有获准作用域为 `unsupported`。SDK 将存储及其它未识别的本地状态失败映射为 `Trapped`，不伪装为上游响应解析错误；未处理的 CAS 冲突同样保留独立错误信息，不授权请求重放。
- `journal` 仅供已准入的 conversation cell 使用。正文沿用 ContentStore，cell 原子关联 payload、revision 与不可变 journal head；checkpoint／fork 共享已存在的正文引用，不复制累计大正文。单条链、同一会话作用域发布的累计正文与单个操作暂存正文受 64 MiB 配额约束，超限失败而非截断历史。append 不表示发布；只有成功 CAS 使新 head 成为状态，失败分支的未引用内容由现有租约与 GC 回收。
- 成功改变 journal head 的 CAS，在同一事务中关联宿主提供的 History Boundary Candidate。`owner_admission` 是当前发布者，canonical 来源可以是获准祖先；重复内部尝试不会把 head 回退到旧位置。该关联只证明投影位置，不证明请求已经发出或上游已接纳。重复文本、工具循环与同一 gap 的多个有序变更不以内容 hash 猜位置；codec 在一次编码中把 canonical gap 映射为实际 native input offset。
- checkpoint、fork、原生压缩与失败 admission 继承确定的状态快照。历史清理通过持久 epoch 使旧操作失效，GC 保留仍被活跃租约、Generation、compaction 或 checkpoint 引用的内容；不会因为清理一次查询记录便删除仍可恢复的历史。SQLite、PostgreSQL 与内存适配器遵守同一行为契约。已有 Provider fence 的读取不获取写事务；首次创建时才串行重检。
- `volatile.open` 使用相同 key／payload／CAS 契约，但仅存在于进程内，并按持久安装身份、Vendor、Provider、数据 epoch 与 fence 隔离。进程重启后为空；同一进程内重建 Gateway 不重置仍有效的值。本进程的 reset／删除／卸载使旧 handle 失效；跨进程更新在下一次 fence 校验被观察到，不承诺立即擦除另一进程已持有的内存。网络 I/O 不持有数据库事务。

Devin 的 native step 与 cascade assignment 是有效上游会话绑定，不按固定项数清空或猜测过期。它们保存在 Provider CAS cell；仅从未发布过的 cell 首次导入旧 private state，初始化冲突后读取已发布值，之后不再读取或写入旧业务来源。旧 private blob 与 OAuth 共用且不支持 CAS，因此不为清理业务字段覆盖它。没有稳定 affinity 的请求使用一次性身份，不持久化无法复用的绑定；有 affinity 的并发更新合并，达到 cell 容量时显式失败并保留既有绑定，不重放外部请求。

基础插件的云凭据指纹缓存最多保留 64 项，移除已过期或进入现有五分钟续期窗口的 token；满时优先淘汰最早失效的其他项，保留本次取得的 token。这类可重新获取的凭据缓存与不可随意淘汰的会话绑定区别处理。

宿主的 `operation_metadata` 提供已证明的原生请求事实，而不是信任客户端自报的 CLI 标头：

|事实|生命周期|
|---|---|
|`client_installation_id`|安装初始化时原子生成并持久化的随机 UUID；同一安装跨进程与重启稳定，不采集真实硬件、用户名或目录。|
|`user_prompt_index`|获准会话链路的逻辑用户输入序号；同一输入的重试、failover 与工具后续调用复用，压缩和重启不从截断历史重新计数。真正缺少旧来源证明时保持未知，不伪造 1 或 2。|
|`client_conversation_id`、`client_conversation_created_at_ms`|获准会话的持久身份与创建时间；受 Principal 和 lineage 隔离，可信分支继承会话，不采用客户端自报的 session 标头。|
|`user_prompt_id`、`user_prompt_created_at_ms`|获准用户输入的独立身份与创建时间；纯工具结果和内部重试继承，新用户输入及并发分支分别分配，重启不重新生成。|
|`logical_turn_id`、`logical_turn_started_at_ms`|本次逻辑用户轮的身份与起始时间；不以每次 HTTP 尝试或每次模型工具循环重新生成。|
|`context_window_id`、`context_window_index`|有来源证明的上下文窗口及替换次数，与用户轮次不同；压缩安装新窗口后推进。|
|`history_boundaries`|当前送入 guest 的 canonical 视图中的已保留 gap 身份和当前位置；删除 reasoning 等转换同步投影位置，不以转换后的内容重新认定身份。|

Generation Chain 的 `user_prompt_index` 统计含用户文本或媒体的用户项，排除纯工具结果；Claude 的 `user_turn_index`／`dialogue_prompt_index` 保留其独立的用户轮投影，混有工具结果的项不推进这两个计数。Agent 的用户轮由 Agent Turn 来源证明，不把每次 Model Turn 当作用户输入。已有旧历史没有所需证明时，要求该事实的插件明确报错；普通完整历史与有证明的续接路径不因此退化。

直接调用 Model Turn 且没有来源准入时，宿主建立新的受 fencing 保护的独立 lineage 与逻辑轮／窗口身份；只有经当前 Principal 验证的压缩记录可以带入父 checkpoint。已有完整准入原样复用；只传身份而不传准入、或会话上下文与租约不完整时拒绝。独立准入不虚构用户计数或历史边界。

并列的失败前缀不能授权任一 sibling 的 checkpoint、窗口或私有状态；没有明确已提交祖先时，宿主开启隔离的新 lineage，计数保持未知，普通 API 推理仍可继续。要求这些计数的 CLI 插件仍会明确拒绝此类无证明上下文，不宣称每个 Vendor 都能在该场景恢复。历史清理尝试所有独立步骤并记录、聚合全部失败，不因第一个失败吞掉其它原因。

### Net 1.x 的错误与类型稳定性

`stravia:net@1.0.0` 独立提供 HTTP、WebSocket 和 operation-scoped options 资源。字段为保留次序、大小写和原始值字节的 `field { name, bytes }` 列表；未知选项由宿主在发送前返回 `unsupported-option`。SDK 可以保留未来选项键，但本地构造成功不等于宿主支持该选项。

`net-error.code` 是开放字符串。已定义代码如下：

| 代码 | 契约 |
| --- | --- |
| `unsupported`、`unsupported-option` | 当前宿主不支持所请求的能力或选项；不隐式忽略。 |
| `invalid`、`invalid-request`、`invalid-response` | 输入或请求不合法，或收到无法按契约解析的响应。 |
| `auth`、`provider-not-found` | 网络或操作准入被拒绝，或指定 Provider 不存在。 |
| `continuation-not-found`、`continuation-unavailable`、`protected-reasoning-rejected` | 保留宿主既有续接、回放与受保护推理判定，不据此生成通用重试。 |
| `upstream` | 可以携带 status、retry-after、model-error-kind 与 transport-failure 事实；重试与 failover 决策仍归宿主。 |
| `trapped`、`cancelled`、`deadline-exceeded`、`resource-exhausted` | 保留独立生命周期失败，不能因为附带上游状态或重试提示而变成可重放请求。 |

未知错误代码保守处理为无法解析的响应，不默认可重试。`transport-failure` 保留开放字符串，但未知值不建立 WebSocket 专属回退安全性；`model-error-kind` 使用带 `other(string)` 的 variant 保存未知模型错误。`operation-kind`、`error-kind` 与既有 record 布局保持封闭；修改它们需要主版本升级。minor 只增加独立可选接口或开放键值能力，不增加必需 export。

已发布快照保存在 `backend/crates/stravia-vendor-sdk/wit/released/`，`index.json` 锁定每个 WIT 文件的 SHA-256；包括 Vendor 1.0.0 及其依赖和独立 Net 1.0.0。发布新 minor 时保留旧快照原样，新增版本化快照及其依赖、更新索引，并运行 `cargo run -p stravia-devtools -- wit-compat` 检查已发布内容及向后兼容；不得修改已发布 record、variant、enum 或已有签名。版本准入不能代替真实旧组件加载及 Linker 类型检查。

### 实现与验证入口

- 所有 crate（包括 Wasm 契约 fixture）的 package 版本与依赖统一由根 `Cargo.toml` 的 workspace 声明，依赖解析由唯一的根 `Cargo.lock` 管理；子清单继承 workspace，不维护独立版本、依赖版本或锁文件。
- SDK 的 `fixtures/` 下包含 capability、lifecycle、management、manual、net-1.0-compat 与 state 六类 Wasm 专用工作负载，通过 `task build:vendor-fixtures` 构建。manual fixture 不依赖 SDK guest 封装，验证 HTTP 协商、原始响应、预编码请求及无压缩 WS；net-1.0-compat 使用冻结快照验证旧 1.0 组件，state fixture 验证实际资源调用与 CAS。常规 host clippy 与 unit lane（包括测试预编译）显式排除这六个 fixture crate，不在 host 上编译它们；unit lane 仍排除 `stravia-desktop`。这些兼容检查不承诺已移除的 0.x ABI。
- `stravia-core/tests/vendor_plugin_lifecycle.rs` 的 CPU 死循环回归使用受控 `RequestContext` 调用真实 Responses handler，在 guest 发出 Started、首个 attempt 合并进 Model Turn 开始事件后，主动过期共享 deadline。它断言 HTTP 504、无已知用量和同一插件仍能处理健康调用，不等待或修改产品默认的 300 秒空闲时限；协议选择副作用等插件故障仍严格断言 HTTP 502。
- `backend/crates/stravia-vendor-sdk/` 提供 Rust SDK 与独立版本化的 Vendor 1.0、Net 1.1、State 1.1 WIT；`stravia-runtime-contract` 提供 canonical 类型，`stravia-protocol-codec` 提供四类标准 codec 与通用 canonical 转换辅助。
- `backend/crates/stravia-vendor-runtime/` 实现 Component 执行与受控资源；Core 的 `src/plugin/` 负责安装、连接快照、网络授权、私有状态及版本切换协调。
- Core 的 `src/plugin/network/transport/` 使用 Rama 和打补丁的 rustls 实现连接、代理和 HTTP 传输，`shape/` 校验内置 Egress Shape 与 TLS 配置；补丁清单还包含 Rama HTTP core 与 types，构建前须准备补丁。`stravia-devtools/src/wit_compat.rs` 检查快照及兼容性，runtime 的 `tests/wasi_compatibility.rs` 包含版本准入和兼容组件加载检查，`stravia-vendor-sdk/fixtures/net-1.0-compat/` 使用发布快照。
- 同一 Engine 的运行时克隆可复用相同完整 SHA-256 字节身份、仍被已加载版本持有的不可变编译 Component，并合并同一身份的并发编译。缓存只保存有界元数据与弱引用，不延长退役版本的生命周期，也不缓存加载失败；每次加载仍检查 imports，并使用新的 Store 实例读取和验证 descriptor。操作的 HostServices、私有状态、取消与权限边界不共享。
- 每个已加载版本在检查 imports 后准备一次 typed `VendorPre<StoreState>`，复用导入解析与导出索引；descriptor、协议选择、执行和 catalog 同步统一从该 blueprint 实例化，但每次仍创建独立 Store、HostServices 与资源表。blueprint 绑定组件原 Engine，因此由另一 `VendorRuntime` 执行时也不能改用调用方的 Engine。`with_descriptor` 同时保留 blueprint 和外层 `Arc<Component>`；后者让已有弱编译缓存继续有效，全部版本句柄释放后仍可回收，不改成强引用常驻缓存。
- 持久运行时的首次装载与空闲恢复共用不可变编译镜像缓存：以完整 Wasm 身份及 Engine 兼容性区分产物，校验长度和 SHA-256 后通过 `Component::deserialize_file` 映射执行。发布与清理由跨进程文件锁协调，只原子替换目录项，不修改仍被映射的文件内容；缺失或损坏的可信产物重新编译，不可信条目不读取或覆盖。干净文件页可以回收，但 Store、guest 线性内存及宿主堆不因此移到磁盘。Core 使用 `<data-dir>/cache/wasmtime-images`，不自动删除旧压缩缓存；目录信任、回退与软容量见 [架构：Wasmtime 编译缓存](architecture.md#wasmtime-编译缓存)。`runtime::compilation_cache_tests` 覆盖源身份、Engine fuel 配置、并发发布、损坏恢复、不完整校验记录下的不可信条目保护，以及淘汰后既有映射继续执行。
- `LoadedPlugin` 把轻量的 descriptor 与内容身份同可释放的编译产物（`Component` 与 `VendorPre`）分开。运行时带持久编译缓存时，调用方通过 `with_source(ComponentSource)` 绑定字节来源（Core 为已安装插件的 artifact 和内嵌组件各提供一种），已装入的产物沿用，每次操作持有租约，所有租约归还且空闲满 60 分钟后释放；释放不影响正在执行的操作。恢复重新读取字节，核对完整 SHA-256 后才编译，再次检查 imports；来源返回不同字节、读取失败或编译失败都使该次操作失败，不改用其他代码。并发恢复串行化为一次，等待恢复同样服从取消与截止时间。没有持久编译缓存的运行时忽略 `with_source`，插件保持常驻。`with_descriptor` 共享同一驻留，因此版本切换与描述符覆盖不会复制编译产物。`with_admission_records(目录)` 启用准入记录：`load` 成功后 `record_admission` 写入组件自身的原始 descriptor，`restore(摘要, 来源)` 在记录有效且运行时带持久编译缓存时返回尚未装入的句柄，否则返回 `None`，调用方回退到 `load`。句柄首次装入时完整检查 imports，并要求组件报告的 descriptor 等于记录，不等则该次操作失败并作废记录。真实 base Component 的驻留与准入记录回归为显式 opt-in：先执行 `task build:vendors`，再运行 `cargo test --locked -p stravia-vendor-runtime --lib residency -- --ignored`；这些用例也纳入 `task test:all` 的本地 opt-in 清单。
- `stravia-vendor-runtime/tests/wasi_compatibility.rs` 通过公开加载接口检查权限边界、预发布版与其他版本系列拒绝，以及非法 SemVer 的组件校验失败。真实 base Component 的多个稳定 patch、build metadata、混合版本加载和协议选择回归为显式 opt-in，并验证新 patch 要求宿主不存在的函数时加载失败：先执行 `task build:vendors`，再运行 `cargo test --locked -p stravia-vendor-runtime --test wasi_compatibility -- --ignored`；该用例也纳入 `task test:all` 的本地 opt-in 清单。缺少 manifest 或产物时明确失败，不静默跳过。测试重定向实际 Component 的顶层 WASI import 名称，保留 guest 实现和内部元数据，并保持跨接口资源族的版本一致。
- `backend/crates/stravia-vendor-base/`、`stravia-vendor-codex/`、`stravia-vendor-grok/`、`stravia-vendor-command-code/`、`stravia-vendor-devin/`、`stravia-vendor-claudecode/` 等 crate 是 guest 实现来源；只有 `stravia-vendor-base` 进入 Core 的默认内嵌集合。
- `backend/crates/stravia-vendor-common/` 是只提供多个 guest 实际共用辅助代码的 `rlib`。标准 codec 由 guest 在 Rust 源码层链接 `stravia-protocol-codec` 并编入 Component，而非采用 Component composition；Command Code 与 Devin 私有 codec 留在各自 crate，Bedrock、Cohere、Gateway、WatsonX 等私有实现留在 `base`。
- 默认 builder 与 `task build:vendors` 只构建 `base`，在 `target/vendor-plugins/manifest.json` 生成仅含 `base` 的 manifest，供 Core 内嵌。发布流程通过 `task build:vendors:all`（builder 的 `--all` 模式）在独立的 `target/vendor-plugins-all/manifest.json` 生成全部 Component 的完整构建 manifest，并将其中专属 Wasm 以 `stravia-vendor-{vendor_id}-v{version}.wasm` 独立 Release 附件发布，统一由 `SHA256SUMS` 覆盖，供本地导入；`base` 不作为专属附件重复发布，完整构建 manifest 也不发布。两种模式都单独编译并立即归档 `base`，再构建专属插件，避免 Cargo feature 合并改变 `base` 字节、使开发启动将其识别为本地替代包。`task build:vendor-fixtures` 依赖完整构建并生成真实测试组件。Core 的 `vendor_*` 契约与生命周期检查通过 Gateway 和本地上游验证行为；浏览器、Desktop 与双数据库验收仍分别使用根 `Taskfile.yml` 中的对应入口。
- `task dev:server` / `task dev:desktop` 启动前构建全部仓库插件，并通过 `STRAVIA_DEV_VENDOR_MANIFEST` 在 debug Gateway 初始化时自动安装或更新这些本地产物。首次设置完成后同样生效，不需要先逐个手动导入。相同摘要不重复安装；同版本不同摘要也会更新，`base` 保持内嵌来源。需丢弃已有数据的更新仍跳过并记录警告，按日志产物路径手动导入并确认；发布构建不包含这条开发路径。源码变更后重新运行开发任务，不自动热更新运行中的插件。Core 的 `plugin::development::tests::` 覆盖同版本构建替换、重复启动幂等与不兼容数据保护；真实开发任务 E2E 同时验证局域网入口、认证边界和完整插件清单。
- Core 的 WebSocket 续接回归位于 `src/proxy/dispatcher/inference_run/tests/websocket_continuation.rs`，使用内嵌 `base` 的标准 OpenAI Responses 通道与本地 WebSocket 上游，验证连接复用、精确 tip、淘汰、并发分支、完整历史恢复和重试预算，不安装或加载 Codex。构建 `base` 后，可通过 `cargo test --locked -p stravia-core websocket_continuation::` 独立运行，不要求构建专属插件。
- Codex 的真实 Component 握手、错误分类和大推理签名回归归属 `stravia-vendor-codex/tests/websocket_contract.rs`，不放入通用 runtime 测试。它们默认标记为 ignored；先运行 `task build:vendors:all`，再通过 `cargo test --locked -p stravia-vendor-codex --test websocket_contract -- --ignored` 显式验收。显式运行时缺少产物会报错，不静默跳过。

## 已确认的管理与分发范围

- 提供插件管理页面，在其中展示已加载的插件。
- 插件管理页面提供本地包安装与更新入口，并展示更新结果；普通运行时只有内嵌的 `base` 可随宿主升级发生内置插件自动更新。
- `base` 由程序内嵌；普通运行时的专属插件仅允许管理员通过本地插件包安装或更新，开发任务可自动安装当前仓库构建的本地包。
- 当前不做插件市场，不提供在线插件获取流程。

### 插件卸载

- 管理页面为 `base` 之外的已安装插件提供卸载入口，包括加载失败、产物缺失以及历史上来自程序随附包的专属插件。`base` 不能卸载；其恢复流程仍使用“恢复内置版本”。
- 卸载前确认插件名称及影响：依赖该插件的供应商和能力将不可用；取消该 Vendor 的活动调用和未完成认证会话，但不能保证上游已停止执行或计费。
- Core 先停止新任务准入，取消并排空该 Vendor 的操作，撤销旧结果与写回，再移除安装记录和运行态。卸载不需要成功加载组件，也不依赖损坏组件的描述符。其他 Vendor 的调用和无关更新预览保持不变。
- 卸载保留 Provider 配置、凭据、模型、Route/Target 绑定、历史、用量、插件私有数据与恢复标记，不自动删除或改绑；内容寻址产物文件也不在此次操作中清理。卸载相关的旧更新预览失效，避免重装后重复使用旧确认。
- 重新安装仍通过导入预览；卸载后旧安装描述符不再作为数据兼容基线，保留数据可能需要管理员明确确认重置，不能静默复用无法证明兼容的数据。
- 管理 HTTP 接口为 `DELETE /api/v1/vendor-plugins/{vendor_id}`，沿用现有管理会话与 CSRF 校验；成功返回 `200` 和 `{"data":null}`，失败沿用插件管理错误响应。Desktop 与 Server WebUI 使用同一 Core 行为。

## 已确认的产物存储与备份边界

- 程序内嵌的 `base` Component 直接从进程内存加载，不把其 Wasm 字节写入 `<data_dir>/plugins/artifacts/`。本地导入的专属插件或 `base` 替代包则作为不可变、内容寻址文件保存在 `<data_dir>/plugins/artifacts/<sha256>.wasm`；任何 Component 字节都不存入 SQLite 或 PostgreSQL，也不在持久化记录中保存任意外部文件路径。
- SQL 只保存摘要、来源、版本、revision、epoch 等安装元数据以及业务数据和插件私有状态。供应商 Profile ID 与连接 UUID 分列保存，不能用 `base` 包身份替代 Profile 归属或把两者误设为同一外键。
- 本地包安装与更新必须先校验、写入并同步产物文件，再提交使新 revision 生效的数据库元数据；任一步失败都保留旧产物与旧安装记录，失败的新 revision 不生效，不能先覆盖旧版本。内嵌 `base` 的发布直接使用程序内字节，但仍须先完成同等校验与加载准备。旧插件若与当前宿主契约不兼容，仍不得继续执行。
- 完整备份与恢复必须同时包含数据库和本地导入产物所在的 `<data_dir>/plugins/artifacts/`；内嵌 `base` 由程序二进制提供，无需写入或备份为实例产物。这对 SQLite 与 PostgreSQL 同样成立：存在本地导入插件时，只复制 SQLite 数据库文件或只备份远程 PostgreSQL 都不构成完整实例备份。
- 数据目录迁移工具随现有数据目录一起复制 `plugins/artifacts/`，不另增独立插件路径参数；存在本地导入记录时，迁移完成前不能把仅有 SQL 元数据的目标实例视为可运行恢复。

## 已确认的配置表单与认证交互

- 插件只声明配置结构，不携带供管理面执行的 JavaScript、HTML 或 Svelte 组件；宿主统一呈现表单，Desktop 与 WebUI 共用配置契约。
- 字段支持布尔、字符串、整数、小数与枚举，声明 key、名称、说明、默认值、必填及范围等约束，并允许必要分组与简单条件显示；不通过表达式脚本实现任意界面逻辑。
- `ConfigField.visible_when` 可以引用其他配置字段，也可以通过保留上下文 `$protocol` 与协议选择标识精确比较，例如 `{"field":"$protocol","equals":"openai-compatible"}`。该上下文不写入 `vendor_options`，插件不能声明同名配置字段。隐藏的父字段同时隐藏依赖它的子字段；宿主的必填校验使用相同条件。配置预览可显式传入 `protocol`，未传入时采用已有连接或 channel 的默认协议；显式选择仍须符合插件声明的协议枚举。
- 普通选项与秘密凭据分开。通用字段约束由宿主检查，跨字段业务约束由 Wasm 插件校验；前端展示校验结果，不复制供应商业务规则。
- 支持配置校验的插件可以提出派生地址；内置云供应商仅在地址为空时生成建议，保留管理员显式填写的地址。管理面必须先展示最终地址及其精确 origin，再由管理员确认保存，修改候选配置会使旧审阅失效。OAuth 登录仍要求管理员明确提交非空地址，不把尚未展示的校验建议当作网络授权。
- Command Code 的 zdr 等供应商选项通过同一声明机制呈现、校验和保存，不在宿主前端添加供应商专属表单代码。
- OAuth 使用宿主提供的打开授权链接、输入授权码、等待完成等标准交互；供应商登录网站在浏览器打开，不作为插件自带页面嵌入管理面。
- 此限制不缩减插件对 OAuth 请求构造、token 交换与供应商响应解析的所有权，也不授予插件访问管理会话的能力。
- 宿主在 `AuthStep::Start` 中提议 `state`。上游要求特定格式时，插件可在 `AuthResponse::Authorization.state` 返回替换值（仅 URL 非保留字符 `A-Z a-z 0-9 - . _ ~`，1–256 字节，不合法则拒绝创建会话），宿主随后按该值校验回调。该字段是认证结果 JSON 中的可选字段，不改变 WIT 版本；不返回该字段且符合当前 WIT ABI 的插件继续使用宿主提议的 `state`。返回该字段的插件在不认识此字段的旧宿主上会因回调 state 不一致而登录失败。

### 已确认的动态表单多语言契约

动态表单多语言使用当前 `stravia:vendor@1.0.0` 契约，描述符中的本地化文本形状不变；这不表示旧 WIT ABI 仍可准入。语言只改变展示，不改变配置含义、校验规则、网络授权或插件执行行为。

#### 文本所有权与结构

- 插件作者通过语言文件维护文案，每个 guest 包使用 `messages/en-US.json`、`messages/zh-CN.json` 等扁平消息目录；稳定的消息 key 与翻译分离，Rust 业务代码不内联中英文文本对，也不按英文字符串查找翻译。
- 所有 guest 共用 SDK 提供的构建期消息生成器。构建时校验语言目录，并为英文目录中的消息 key 生成有类型的 Rust 函数，将文案编入自包含 Wasm；错误消息 key 在编译时失败。运行时不解析语言文件，不从磁盘或网络加载语言包，安装包不需要附带额外资源文件。
- 各包的 `build.rs` 调用 `stravia-vendor-sdk/build-support/messages.rs` 中的 `generate()`，源码包含生成的 `OUT_DIR/messages.rs`。消息 key 使用合法、非保留字的 snake_case Rust 标识符；`{field}` 等具名参数生成显式的 `&str` 参数，`{{` 与 `}}` 表示字面量大括号。Core 的通用约束错误使用自身的 `messages/` 目录和同一生成器。
- 插件随描述符或校验结果携带解析后的完整多语言文本，而不是向宿主发送消息 key。宿主统一选择语言和处理回退，不按供应商身份补翻译，不把插件文案编入 WebUI 的 Paraglide 消息目录。动态新增 Provider Profile 不需要重新构建 WebUI。
- 使用唯一的 `LocalizedText` 对象结构，不接受 `string | object` 双形态。键为语言标签，值为纯文本；`en-US` 必填且非空，作为规范默认值。其他语言可以缺省，但声明后必须具有有效语言标签和非空文本。仓库维护的插件表单文案同时提供 `en-US` 与 `zh-CN`。
- 可选说明没有内容时使用 `null`，不能用空翻译隐藏某个语言下的说明。文本不作为 HTML 或脚本执行；不引入远端语言包或运行时插件模板引擎。需要动态参数的消息使用构建期检查的具名占位符，生成器要求各语言的参数集合一致，并生成显式参数函数；不支持任意表达式或 ICU 逻辑。
- 构建期拒绝重复消息 key、空文本、无英文基线的翻译 key、非法语言标签与参数不一致。非英文目录可缺少个别消息，生成结果保留英文回退；仓库维护的正式插件文案仍须同时提供中英文，测试回退的夹具除外。

例如，`messages/en-US.json` 与 `messages/zh-CN.json` 分别定义同一个 `zero_data_retention` key，插件通过 `messages::zero_data_retention()` 取得 `LocalizedText`。以下示例描述传给宿主的结构，不是 Rust 代码中的文案维护方式。

```json
{
  "key": "zdr",
  "label": {
    "en-US": "Zero data retention",
    "zh-CN": "零数据保留"
  },
  "description": {
    "en-US": "Request zero data retention from the service.",
    "zh-CN": "向服务请求零数据保留。"
  },
  "kind": { "type": "bool" },
  "required": false,
  "default_json": false,
  "group": "privacy",
  "secret": false
}
```

以下展示字段使用同一 `LocalizedText`：配置字段的标签与说明、枚举及协议选项的标签、分组标题、Channel 名称与说明、OAuth 手动输入标签与说明，以及插件返回的表单业务校验消息。保存、取消、必填、请选择等宿主通用文案继续通过 Paraglide 提供。配置 key、枚举 value、协议 ID、错误 code、URL 与用户输入保持原值。

#### 分组身份

Provider 描述符通过 `config_groups` 声明分组，每项包含稳定的 `id` 和多语言 `label`；`ConfigField.group` 只引用该 ID。宿主检查分组 ID 唯一且字段引用有效。分组归属与渲染 key 使用 ID，不使用翻译后的标题；语言切换不得改变分组、字段顺序或导致控件重新挂载。

```json
{
  "config_groups": [
    {
      "id": "privacy",
      "label": {
        "en-US": "Privacy",
        "zh-CN": "隐私"
      }
    }
  ]
}
```

#### 语言选择与状态保持

- 宿主通过统一的 `resolvePluginText(text, locale)` 入口解析文本，规则为“当前界面语言精确匹配 → `en-US`”。不自动将 `zh-TW` 当作 `zh-CN`，也不选择映射中的第一项；未来扩展语言匹配策略时统一修改该入口。
- Core 管理接口传递完整多语言结构，不按 `Accept-Language` 改写共享描述符或缓存。Desktop 与 Server WebUI 使用同一结构与解析规则。
- 界面语言变化只重新派生展示文本，不重新请求描述符、不调用 Wasm、不重新执行校验，也不重建表单草稿或重新应用默认值。
- 切换语言必须保留用户输入、枚举选择、秘密字段状态、校验结果、焦点和已完成的地址审阅；不得触发保存或改变网络授权。

#### 校验反馈

- 插件的 `ValidationIssue.message` 使用 `LocalizedText`；`field` 保持配置字段 key，`code` 保持稳定机器标识。已返回的错误可以直接随界面语言切换，不再次执行插件。
- 通用字段约束由宿主校验，相关错误使用宿主消息目录；跨字段业务约束仍由插件校验并提供翻译。WebUI 不按供应商或英文错误字符串推断翻译，也不复制业务规则。
- 原始上游错误不自动翻译；本次不扩大原始错误或敏感内容的暴露范围。

#### 实施与兼容性

- 这是从 `stravia:vendor@0.3.0` 到 `stravia:vendor@0.4.0` 的破坏性插件契约升级。SDK、五个 Vendor guest、base 运行时目录转换、测试组件、Core 管理链路及 WebUI 消费端统一使用新结构；本地导入的旧插件必须重新构建，不保留新旧文本双路径。
- 即使 WIT 的 `descriptor()` 仍返回字符串，内部 JSON 形状变化也必须纳入契约兼容检查；旧包按既有不兼容机制明确处理，不能继续按新契约执行。
- 安装加载和动态目录刷新使用相同的文本与分组结构校验。新描述符不合法时拒绝发布，更新或刷新失败保留当前有效版本，不覆盖 last-good。
- 文案结构升级不等于配置数据格式升级。不得仅因增加翻译而递增 `config_fields_format`，也不得因此清空配置、凭据或插件私有状态。
- 旧组件不可执行时，更新预览仍从已记录的安装元数据读取配置字段与数据兼容版本，不能因为旧展示字段无法按新契约解析而误判为需要丢弃数据。该元数据读取不允许旧组件继续执行。

#### 验收

1. 安装包含宿主事先不知道的字段名称的真实插件，在中英文下展示字段、分组、枚举、协议选项与 OAuth 输入文案；动态目录新增 Profile 无须修改宿主消息目录。
2. 部分中文缺失时逐项回退英文；缺失英文、声明空文本、重复分组 ID 或无效分组引用时拒绝新描述符，保留当前有效版本。
3. 枚举标签随语言变化，但提交值、条件显示、分组归属与字段顺序不变。
4. 表单填写途中切换语言，草稿、焦点、秘密字段状态与已完成的地址审阅不丢失，不产生保存或重新校验请求。
5. 插件业务校验错误随语言切换，`field` 与 `code` 不变，不重复调用插件。
6. 浏览器与 Desktop 分别检查中英文、长文本和窄屏布局；编译通过不代替实际表面验证。

## 已确认的插件私有持久化状态

- 允许插件保存设备注册标识、账户绑定信息等必要的跨重启私有状态，由宿主管理持久化。
- 插件只能访问当前供应商 Profile ID 与 Provider 连接 UUID 共同限定的命名空间；`base` 的多个 Profile 不能共享私有状态。连接尚未保存时，使用对应认证会话的临时状态，不获得其他连接或认证会话的访问权。
- 不开放任意 SQL、数据库表或文件路径，不允许插件自行修改数据库 schema。
- 配置、凭据和模型清单继续使用各自专用契约，不以私有存储绕过其校验、授权与生命周期规则。
- 插件声明私有数据格式版本，供更新兼容性检查使用；不兼容时按已确认的数据丢弃规则由管理员决定。
- 纯性能缓存优先放内存；活跃请求和连接句柄等运行态不能作为可跨重启恢复的持久化状态。
- 私有状态通过 WIT 的 `read-private-state` / `write-private-state` 整体读取或替换，每个 Provider 或认证会话最多保存 256 KiB。存储操作受任务生命周期约束，被取消的旧版本任务不能继续写回，超过容量的替换不能覆盖已保存状态。

## 已确认的供应商流程与网络执行

- 插件允许主动调用宿主提供的受控 HTTP / WebSocket Interface，自行编排供应商流程，不强制通过返回 RequestPlan 让宿主逐步推进。
- 插件拥有供应商端点选择、请求编码、响应解析和辅助调用顺序，覆盖 Devin AssignModel、OAuth、模型发现、额度查询及正式推理。
- 宿主执行实际网络操作，统一应用代理、目标访问限制、取消、deadline 与诊断。插件不能创建绕过宿主的原生 socket。
- 供应商网络客户端不另设固定的 HTTP 总时长上限，默认连接与显式代理遵循同一操作 deadline。推理入口的共享 deadline 默认在 300 秒无插件与宿主边界活动后到期，有活动则续期，因此持续输出可以超过 300 秒；显式固定 deadline、首 token 超时和取消仍按各自契约生效。
- 各类操作复用受控网络能力，但权限、凭据范围及资源生命周期绑定对应操作，不能借模型发现或额度查询获得其他连接的访问权。
- 模型请求重试、Target failover 与请求重放决策继续归宿主，插件不得自行重放生成请求；供应商辅助调用编排不意味着拥有平台调度策略。
- 首个有效输出前的插件事件按原顺序暂存，每次 Target attempt 使用 16 MiB 的保守占用估算预算，不以事件条数判定异常。预算包含事件槽与预留空间、字符串及数组容量、嵌套 JSON 的动态载荷与对象节点估算；它不是整个请求或进程的精确内存上限。首个有效输出和正常终态触发提交，不因此前缓存达到预算而被拒绝；超预算仍以 `vendor_event_limit_exceeded` 明确终止，不丢弃事件或新增重试。事件的 publication fence、上游错误、deadline、取消与背压语义保持不变。
- Devin 推理出站从唯一 canonical 参数投影已确认的原生 protobuf 控制，不再笼统移除 stop、seed、top_k 或并行控制：
  - CompletionConfig 保留 `max_tokens`、`temperature`（原生零温度限制按既有规则处理）、`top_p`、`top_k`、重复的 stop patterns、可表示为非负 uint64 的 seed、统一 `logprobs` 有值（含 0）时的 return_logprob 开关，以及统一 `service_tier = priority` 对应的原生字符串 `"priority"`；其它服务等级未见官方原生语义，静默省略。
  - `parallel_tool_calls` 反向编码为请求字段 #11 的 `disable_parallel_tool_calls`；`tool_choice: auto` 与合法具名工具分别编码 #12 的 option/tool name，`none`、`required` 与其它无支持选择省略。不能据此承诺强制或禁止工具调用，也不把被省略的选择改写成提示词。
  - Effort 由模型 selector 消费，不是额外 wire thinking 字段。presence/frequency penalties、`response_format`、非 Effort thinking control，以及候选数、logit bias、top-logprobs 数量、prediction、verbosity、background、max-tool-calls、truncation、结构化输出、外来缓存/压缩等未确认原生字段省略；协议原始扩展不能绕过白名单。具体字段布局与投影见 [Devin 私有编码器](../../backend/crates/stravia-vendor-devin/src/codec/devin_connect/request.rs) 和 [准备入口](../../backend/crates/stravia-vendor-devin/src/codec/devin_connect/mod.rs)，遵循 [ADR-0006](../adr/0006-own-protocol-conversion-behind-canonical-stages.md)。
  - 受保护思考写入 prompt #12：`devin-thinking-v1:` wrapper 先解包，signature type 写入 #18；其它非空载荷（包括其它上游的 Responses `encrypted_content`）作为不透明签名先试一次，被拒后由宿主按 ADR-0080 剥离外来载荷。redacted 状态使用原生载体；没有受保护载荷的 summary/content 仅按可读助手正文投影，原始历史不改写，不制造签名。
  - 原生 trajectory/cascade 只由宿主主体+Target 作用域的完整 `transport_affinity` 按上方固定 namespace 派生，不再依赖会话 token；正常刷新不会重命名同一宿主会话。缺失作用域时每次请求生成新身份。canonical session 可参与宿主的作用域身份计算，但客户端原始 session/cache/correlation ID 与 prompt 内容不能直接决定复用。实际 step index 由插件持久状态推进，AssignModel 的 JWT 与同一 cascade 绑定，不从客户端伪造；首次切换新 trajectory，需重新构建并导入 Devin Wasm。官方 3000.11.3 的精确 ID 版本仍未证实，不把既有 v4 codec 形态当作官方服务端要求。
  - 消息、媒体、工具定义、工具调用及关联结果和原生压缩状态，目标能表达时保留，不能表达时默认省略；请求、响应、stream delta 与 continuation 历史均适用，不因语义损失整体拒绝。统一原生工具与 Custom／Namespace 工具不会因此伪装成 Devin 工具。同步执行省略不支持的 background 控制，不承诺异步任务；其他供应商同样遵循默认省略政策。该规则不绕过源协议内容解析、安全、凭据或网络校验。
- Devin 仅在上游模型标识首次出现或变化时生成对应 metadata 事件；重复标识不跳过同帧用量或其他内容。宿主不依赖这一去重来接收长 metadata 前导流，也不统一合并不同协议的 metadata、用量、思考签名或其他增量。
- WebSocket 失败通过强类型传输事实跨越 WIT 边界，不直接授权重试。只有未提交结果且既有 Route 策略允许同 Target 重试时，宿主才将下一次尝试设为 HTTP-only，并去除仅当前 WebSocket 可用的续接状态，以完整历史重放；下一次客户端请求恢复自动传输选择。
- 插件可在 `ws.reuse-max-age-millis` 中声明连接从首次握手成功起算的最长复用时长；缺省使用宿主通用的 60 分钟，宿主只会把声明值限制在该上限内，`0` 表示不跨请求复用。Codex 插件声明 55 分钟，为上游一小时寿命预留 5 分钟余量；宿主不识别供应商。闲置连接由既有池过期任务主动释放；在途响应不受该门槛截断，完成后不再归池，原有 60 分钟活动连接边界保持不变。反复复用或后续请求都不能延长既有连接的寿命。下一次请求通过下述完整历史回退重新建连，不把旧连接的 `previous_response_id` 带到新 socket。
- `ws.continuation-id` 只用于依赖连接本地状态的续接。宿主在池锁内匹配精确身份和 response tip、独占取出 socket，再检查其可用性；找不到时返回 `continuation-unavailable`，不新建握手、不发送增量请求。完整 Effective Model Request 仍由 Executor 保留；插件将此事实返回宿主，由宿主统一恢复，不消耗上游恢复预算或增加 Target 健康失败计数。
- 插件消费成功的 `completed` 终态后，通过 `ws-connection.close(Some(upstream response ID))` 归还连接；宿主不解析供应商 JSON 来猜测身份。`close(None)` 清除旧 tip；取消、中途断流、非法帧或未知终态直接销毁连接。`store=false` 的旧 ID 不得发送到新连接、其他分支占用的连接或 tip 已前移的连接。
- 续接恢复由宿主 Model Turn 唯一拥有。插件每次 Infer 最多发送一次模型请求，只报告未发送的 `continuation-unavailable` 或明确未执行的 `continuation-not-found`；宿主在 Client Output Commit 前最多恢复一次，Compact 不恢复。完整历史由宿主保留，正常路径只共享 `Arc`；恢复时使用原有完整请求，或取回优化为 delta 前的快照，并清除失效 ID。仅缓存的 `MessageStart`／`ResponseMetadata` 等控制元数据随失败 operation 丢弃，不混入重放结果。`operation_metadata.continuation_replay=true` 是宿主下发的本次全量派发标记，不能从客户端自报值取得，不是插件申请重试的接口。
- 续接拒绝与完整重放共用同一个逻辑 attempt，宿主按最终结果统一结算：成功不留下失败记录，失败只记录最终真实错误。恢复不占 Target Retry Budget，预算为 0、半开与冷却额外 attempt 同样适用；新的插件操作必须重新发出 Started、通过全部资格门禁并扣目的地 RPM，不重置总 deadline、不退回旧额度。普通上游错误、执行状态不明和已有输出提交的错误不能归入安全续接恢复。
- 完整历史重新派发时，宿主按原 Principal 和所选 Target 的协议物化媒体，包括历史与新增的嵌套工具结果；读取、权限校验和交付形式复用正常派发路径。`stravia://artifacts/...` 是宿主内部引用，不能直接发给上游。物化仅发生在交付副本上，不改写持久化 canonical 历史，也不让正常增量续接读取完整历史媒体；失败时终止回放，不发送未还原的引用。
- 已移除实验性的 `replay-continuation` host import、SDK 方法和 runtime 入口，不保留兼容接口；依赖旧导入或 0.x ABI 的组件必须重建并重新导入。
- `protected-reasoning-rejected` 仍由宿主恢复，不向客户端暴露未经脱敏的上游错误文本。受保护推理首次不因协议、模型或来源不同提前剥离；被拒后只移除外来与来源不明的载荷并重放一次，本上游载荷始终保留，无可剥离载荷时原样返回错误。该恢复不消耗 Target 恢复预算，也不计入连续失败；重放重新获得完整的 Target 首 token 窗口，总 deadline 不变。重放未再被判为推理拒绝时（包括被首 token 超时等取消，避免后续 Target 重试再次首发同一外来载荷），宿主按“当前签发作用域 × 来源签发作用域”记忆拒绝，来源不明的载荷按摘要记忆；命中进程内记忆时首发剥离，见 [ADR-0080](../adr/0080-try-protected-reasoning-once-and-block-only-foreign-origins.md)。宿主每次派发从实际请求重算 `operation_metadata.protected_reasoning_untrusted`：仅含外来或来源不明受保护载荷时设置 JSON `true`，否则省略。Devin 只在该声明为 `true` 且尚无模型输出时将 Connect `invalid_argument` 分类为推理拒绝；拒绝 trailer 前的模型标识与用量帧不算输出，判定与宿主首个输出提交规则一致；可解包的私有 replay 封装解包，其余非空载荷作为不透明签名先试一次。插件 codec 与内置 codec 一样自行决定可读思考的表示：原生载体能承载无签名明文时原生编码，否则降级为助手正文；宿主不再替插件把思考改写成文本。
- Codex 从可信 `client_conversation_id` 派生稳定 session/thread，窗口为 `thread:N`，其中 N 取持久 `context_window_index`；逻辑 turn 与创建时间取宿主来源证明，内部重试和工具续接不重新计轮。一个插件本地 `NativeSession` 同时投影 HTTP body 的 `client_metadata`、HTTP headers 与 WebSocket 握手；根线程的 root turn 即本 turn，子线程无可信父 turn 时省略 parent/root turn。Responses 与压缩的 `x-client-request-id` 均取 thread，installation ID 取持久安装身份。turn metadata 仅保留明确支持的官方成员，未知自定义键丢弃；自报身份不能覆盖宿主作用域身份。省略的默认服务级别与上游补全的 `default` 生成相同路由提示。认证、账号、URL、插件与凭据作用域仍参与连接隔离。
- 私有 Codex endpoint 不等于公共 Responses API：从[官方请求结构](https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/common.rs)正向投影，省略不支持的采样、输出上限、`metadata` 与 `prompt_cache_retention`，保留已支持的 `prompt_cache_key` 与 `parallel_tool_calls`；客户端未指定缓存键时按官方取 thread ID。HTTP 不发送 WS-only `previous_response_id`／`generate`：若输入仍是增量续接，插件在发出 Started 或网络请求前返回 `continuation-unavailable`，由宿主恢复完整历史后重新派发，不能删 ID 后仍发增量历史。WS 保留官方允许的续接、generate、stream 与 service tier。这些目标投影不改写 canonical 历史。只有实际发送了续接 ID、尚无模型响应事件，且收到明确的 `previous_response_not_found`，或 HTTP 状态语义为 400、`invalid_request_error`、无 code 的精确 ``Invalid `previous_response_id`.`` 时，插件才报告 `continuation-not-found`；普通 400 与已开始响应后的错误不因此恢复。Infer 中的 inline compaction 同样遵守这些安全边界，独立 Compact 不使用续接恢复。
- Codex 请求形态按官方 `0.167.0` 投影。模型 metadata 的 `use_responses_lite` 为真时，HTTP 推理、WS 推理与压缩使用 Responses Lite：顶层 `instructions` 与 `tools` 移入 input 前缀（`additional_tools` item 与带 `model.base_instructions` passthrough 的 developer message，函数工具按官方规则包入 `functions` namespace），并发送 `parallel_tool_calls:false` 与 `reasoning.context:"all_turns"`；HTTP 带 `x-openai-internal-codex-responses-lite: true`，WS 帧改用 `client_metadata.ws_request_header_x_openai_internal_codex_responses_lite`。WS 续接帧只发送新增 item，不重复前缀。非 Lite 模型保留顶层 tools，instructions 同样转为 developer 前缀。推理与压缩请求按模型 metadata 补齐 `tool_choice:"auto"`、默认 reasoning effort 与 verbosity（模型不支持 verbosity 时删除），`include` 固定为 `reasoning.encrypted_content`。HTTP、WS 与压缩统一省略默认 `reasoning.summary:"auto"`，保留非 auto summary 及其它 reasoning 字段，移除因此变空的 reasoning 对象；不再生成默认 `access_programs: {"cyber":"standard"}`，effort 的显式值与模型默认逻辑不变。
- turn metadata 按官方字段顺序生成；环境字段取官方客户端可配置的最小披露：`analytics_enabled` 与 `auto_review_enabled` 为 false，不发送 workspaces 与 tool namespace 信息，`sandbox`／`sandbox_mode` 取 Linux TUI 默认的 `seccomp`／`workspace-write`，node_repl 两项取模型 metadata。`client_metadata` 另带 `root_turn_id`（仅根线程）、`mcp_attribution: {"status":"none"}` 与 `guardian_credits_requested`。推理、模型发现与额度查询统一使用 originator `codex-tui` 与固定 Linux x86_64 User-Agent；额度查询不发送 `x-openai-codex-luna-reserve`，官方仅在能切换 Reserve 模型的客户端声明该头。官方 chatgpt.com 端点的 HTTP 请求体按官方客户端以 zstd 编码，自定义 base URL 不压缩。
- 顶层 body 字段按官方 `ResponsesApiRequest`／`ResponseCreateWsRequest` 声明顺序序列化（路由字段在 `input` 之前，WS 帧以 `type` 开头），仅输出该契约内字段。前缀 item：Responses Lite 对应官方默认开启的 `incremental_tools` 历史路径，base instructions passthrough 依次为 `turn_id`、小数 Unix 秒 `create_time`、`content_item_kinds`、`content_item_metadata`；非 Lite 模型对应官方请求时前缀，ID 为 thread 命名空间（`NAMESPACE_OID` 下 thread ID 的 UUIDv5）内按正文计算的 UUIDv5，passthrough 只有 `content_item_kinds`。
- 与官方 wire 的已知差异及各自的对齐前提见下节「Codex 与官方 wire 的已知差异」。
- 推理与压缩操作的 `operation_metadata.session_affinity` 是宿主下发的上游会话亲和键：只由本地链路根（Generation Chain 根节点或 Agent Run 根 Turn）按主体与 Target 派生为 64 位 hex，同一链路各轮与压缩共享同一值，不采用客户端自报的 session 标头或 `prompt_cache_key`；无链路的操作不下发。`base` 仅为 OpenRouter Profile 将完整源按固定 SHA256 契约投影为其[官方 `x-session-id`](https://openrouter.ai/docs/guides/best-practices/prompt-caching)，不向所有兼容 API 广播同一自定义会话头。专属插件按上方逐字段契约投影，例如 OpenCode Zen 的 `x-opencode-session` 与 Grok 的 `x-grok-session-id`；不合并任意客户端 headers，缺失宿主身份时不从原始别名自行猜测。
- 同一次 Model Turn 的完整历史恢复、认证刷新及随后重放持续复用最初的组件、操作租约与数据代际，不能在恢复循环中重新取得管理器当前版本。真实 HTTP 401 可在未提交且预算允许时进入一次认证恢复；403 不触发刷新。插件声明的辅助认证刷新也由宿主调度，不要求伪造持久化 OAuth 记录。
- OAuth 回放与原生压缩的账户身份采用宿主持久化的稳定认证连接标识，而不是 access token、refresh token 或到期时间。正常刷新保持身份，重新登录创建新身份；非 OAuth 凭据变更仍改变其指纹。Target、供应商、channel、协议、模型、代理、地址与选项边界保持不变。
- 具体职责取舍见 [ADR-0071](../adr/0071-orchestrate-vendors-through-host-network-capabilities.md)，网络授权采用下述元数据声明与白名单原则。

### Codex 与官方 wire 的已知差异

以下为 2026-11-07 对官方 [`rust-v0.167.0`](https://github.com/openai/codex/tree/rust-v0.167.0) 源码与授权抓包的核对结果；表外的 body 字段、header、turn metadata 与前缀形态已按上文投影对齐。WS 握手不带 Lite 头与官方一致：官方同样只把 Lite 标记放在帧的 `client_metadata` 中。

|差异|官方 0.167.0|插件现状与原因|对齐前提|
|---|---|---|---|
|嵌套对象字段顺序|input item、content、工具声明、passthrough 均按结构体声明顺序序列化|顶层 body 已按声明顺序输出；嵌套 JSON 值按键名字母序。官方保持顶层顺序是为了让网关先读到路由字段，嵌套顺序没有已知语义|为每种 item、content 与工具类型维护字段顺序表|
|`generate:false` 预热帧|会话启动且尚无连接时于后台发送一次并等待完成，真实请求以其 response ID 续接增量|不发送|不计划对齐：网关收到首个请求时用户已在等待，模拟预热只会多一次往返、一次上游请求与一次 RPM 发送准入，且插件无法区分会话首轮与历史恢复重放|
|压缩触发源|区分 `auto`／`manual`、`context_limit` 等原因及 `pre_turn`／`mid_turn`／`standalone_turn` 阶段|宿主没有这些客户端内部事实时省略 compaction trigger metadata，不再伪造 manual 默认值；窗口序号由已持久化的宿主窗口证明提供|需要可信操作来源，不能信任客户端自报的 CLI metadata|

WS 新握手携带本次 turn metadata。Codex 在 conversation cell 保存当前 lineage 的有效 tip 与建连快照，随宿主 checkpoint／fork 继承；Provider 共享 volatile 缓存只作查找加速，按序列化 JSON 字节数限制在 1 MiB。成功续接后移除已被替代的旧 tip，写入时先清理超过 55 分钟的快照，容量不足再淘汰最近成功使用时间最早的其他项，优先保留当前响应。没有访问时间的热更新前快照按冷项处理，但仍保留其握手身份。共享缓存被淘汰不删除会话快照，因此会话仍能以原始 metadata 申请同一 socket 的原生续接；宿主仍独立校验精确 tip、身份和连接寿命，不延长连接年龄、不将旧 ID 发到新 socket。消息的 `client_metadata` 继续描述当前逻辑轮。使用此修复需更新宿主，并重建、重新导入独立 Codex 插件。

Cookie 由下述宿主 jar 在真正建连时填充，不把可变 cookie 字节作为连接池身份。Responses Lite 通过 conversation journal 保存首次记录的 `at_`／`msg_` ID、turn、时间及后续工具／指令变更；宿主提供 canonical History Boundary，codec 投影为 native offset，不根据重复文本猜位置。压缩安装新窗口后，`window_number` 与 `thread:N` 随证明推进，不固定为 0。

验证边界：真实上游冒烟覆盖了 Responses Lite 模型的 WS 推理、工具续接、压缩与额度查询；该账号的模型目录均为 Lite 模型，非 Lite 前缀（UUIDv5 ID）只有单元测试与独立计算的参考向量，没有经过真实上游。

### Net 1.x 字段、选项与传输

插件以有序 `field { name, bytes }` 列表交付 HTTP／WS 字段；HTTP/1 在协议允许时保留名称拼写、值和重复字段的交错顺序，HTTP/2 按协议小写化名称，将 Host 转为 `:authority` 并移除连接级字段。宿主不自动添加 `Accept`、`Accept-Encoding` 或 `User-Agent`。SDK 的 `Fields::append` 保留重复项，`set` 替换首项并移除同名后续项，`slot` 写入空值槽位。

Host、Content-Length 和 Transfer-Encoding 是宿主处理的槽位：Host／长度可指定空值，或指定与授权 URL authority／实际正文长度一致的值；Transfer-Encoding 的空值填为 `chunked`。重复或不一致时拒绝，Content-Length 不可与 Transfer-Encoding 同时使用。未指定时按 shape 的 HTTP/1 槽位位置插入必要字段。HTTP/1 的 chunked framing 接收完整未分帧正文；需显式选择 HTTP/1，不假定该字段强制协议选择。WS 的 Upgrade、Connection、Sec-WebSocket-Key、Sec-WebSocket-Version 由宿主填充，key 只接受空槽；Sec-WebSocket-Extensions 和 Sec-WebSocket-Protocol 由插件提供并校验，required 压缩必须提供有效 extension offer。禁止插件提供 Proxy-Authorization、Proxy-Connection；普通 HTTP 禁止 Upgrade，Connection 只允许 keep-alive 或 close，TE 只允许 trailers。

每次 HTTP `start` 或 WS `connect` 借用操作范围内的 options，宿主对每次 setter 独立校验，并在发送时快照。SDK 允许暂存未来键，但宿主对未知键、未知 shape 或非法值返回 `unsupported-option`，不能将本地构造成功当作支持证明。

| 键 | 接受值 | 缺省 |
| --- | --- | --- |
| `shape` | 内置 shape 名称 | `default` |
| `http.version` | `auto`、`http1`、`http2` | `auto` |
| `http.request-encoding` | `identity`、`gzip`、`deflate`、`br`、`zstd` | `identity` |
| `http.response-decoding` | `none` 或上述编码名的逗号列表（不含空格） | `none` |
| `ws.compression` | `disabled`、`preferred`、`required` | `disabled` |
| `ws.transport` | `auto`、`http1`、`h2` | `auto` |
| `ws.reuse-max-age-millis` | 十进制无符号 64 位整数 | 宿主默认 |
| `ws.reuse-only` | `true`、`false`；要求使用已有连接，不允许新握手 | `false` |
| `ws.continuation-id` | 字符串 | 无 |
| `cookies` | 有界 JSON 对象，包含 `domains`、`names`、`prefixes` 字符串数组 | 不使用 Cookie Jar |

插件自行提供与选项一致的 `Content-Encoding` 和 `Accept-Encoding`；非 identity 请求编码必须提供匹配的 Content-Encoding，启用响应解码必须提供允许所选编码的 Accept-Encoding。`none` 不解码正文；启用解码时未知、未声明、重复或叠加的响应编码失败，不回退原始压缩字节。HTTP deflate 使用 zlib 包装。WS 的 preferred 允许未协商压缩，required 要求成功协商；压缩策略不授予重试或网络权限。HTTP 响应、WS 握手元数据和消息仍由各自的操作资源提供，重定向、授权、deadline、取消、重试与 failover 由宿主管理。

#### 进程内 Cookie Jar

`cookies.domains` 只匹配精确的小写 ASCII／punycode 主机名，不隐含子域授权；`names` 与 `prefixes` 区分大小写。策略最多 16 KiB，每组最多 64 项，未知 JSON 字段拒绝。策略只选择 cookie，不扩大既有 origin 权限；cookie 自身的 Domain、Path、Secure、有效期与删除规则由 `cookie_store` 处理。

Jar 按安装、Vendor、Provider、数据 epoch 与 fence 隔离，HTTP 和 WS 共用，字节不进入数据库或磁盘，重启即清空。每个 jar 最多 256 个 cookie、编码总量 256 KiB；同一响应越界时原子回滚本次修改，不清空已有 jar 或截断值。修改仅保存受影响条目的有界回滚信息，不复制整个 jar。

接收的 cookie 在保留前规范化，库忽略的未知属性及原始 Set-Cookie backing string 不进入 jar，预算与实际保留的编码表示一致。非法或不匹配 cookie 的忽略规则不变；HTTP/1 和 HTTP/2 的既有响应头限制约束解析期间的临时分配。

插件可以在有序字段中放置空 `Cookie` 槽位；有匹配值时原位填充，没有值则不发送空头。显式非空 Cookie 优先。HTTP 重定向逐跳重新检查 origin 与策略并吸收合法 Set-Cookie，WS 包括被拒绝握手的响应；已经复用的 WebSocket 不重新握手。连接池身份包含策略而非 jar 当前字节，跨 Provider 仍不得复用。

### Egress Shape

Egress Shape（出站形态）是由宿主校验和执行的连接层数据，不是供应商 Provider Profile，也不包含请求头、URL、凭据或重试策略。它描述 TLS ClientHello、ALPN、HTTP/1 槽位、HTTP/2 SETTINGS／早期帧／伪头顺序及 WS 传输。宿主内置 shape 使用裸名称，数据在 `backend/crates/stravia-core/src/plugin/network/shape/data/`；插件也可在 `VendorDescriptor.egress_shapes` 中打包至多 64 个 `net::EgressShape` 声明，并通过 `shape("plugin:<local-name>")` 选择。local name 仅含小写 ASCII 字母、数字和连字符，同一组件内不得重复，revision 必须为正数；缺省声明为空。插件不能覆盖裸名内置 shape，也不能引用其他组件的声明。

宿主在预览准入和启动恢复时验证、编译声明，编译失败不能确认安装或执行；有效组件与编译后的 registry 作为同一个快照发布。普通操作、catalog sync 和认证会话固定各自取得的快照，兼容更新不把旧组件与新形态混用。HTTP 与 WebSocket 连接池身份包含组件内容身份、shape 名称及修订，不仅依赖可重复的本地名称。插件 shape 复用宿主配置的证书验证器，不增加信任、不扩大 origin 权限，也不提供裸套接字。

Qoder、WorkBuddy 的专属数据、脱敏 capture、采集维护说明及错误码诊断映射分别由各自插件仓库维护，宿主不再内置其名称或专属分类。更新形态仍须更新修订并验证实际 wire；已有通用参数足够时只需重建、重新导入插件，不必发布宿主或修改 WIT。支持新传输原语仍需更新宿主。旧宿主会在选项验证时拒绝 `plugin:*`，不会误用同名旧内置配置；旧插件对 `qoder-cli`／`workbuddy` 裸名称的引用也不再受支持，必须与宿主配套升级。

通用离线命令 `stravia-tools verify-egress-shape --component PATH --shape plugin:NAME --fixture PATH --server-name HOST` 从实际 Wasm 描述符编译并生成 ClientHello，与脱敏 fixture 比较；`--websocket` 选择 WS TLS 配置。命令不连接上游，失败返回非零；它不证明完整 HTTP 请求、真实账号推理或其他平台行为一致。

| 名称（未注明时 revision 为 1） | 数据来源标签与边界 |
| --- | --- |
| `default` | 通用配置；HTTP ALPN 为 h2、http/1.1，WS 为 http/1.1。 |
| `claude-code` | Claude Code 2.1.319，Linux x86_64；HTTP／WS ALPN 为 http/1.1。 |
| `bun` | Bun 1.4.2，Linux x86_64；HTTP／WS ALPN 为 http/1.1。 |
| `bun-1-3-14` | OpenCode Free 使用的 Bun 1.3.14；独立 HTTP／WS 采集，保留该版的组、扩展和 padding，不复用 1.4.2 指纹。 |
| `command-code` | Command Code 1.82.1 真实交互入口，Node.js 24.21.0／OpenSSL Linux x86_64；不使用 Bun 指纹代替。 |
| `devin` | Devin CLI 3000.11.3 真实交互入口，Linux x86_64；独立冷连接与 TLS 恢复样本，扩展顺序随机。 |
| `grok` | Grok CLI 1.0.52 OAuth Responses 真实交互入口，Linux x86_64；独立核对 HTTP/2 SETTINGS 顺序、窗口更新及 ALPN。 |
| `antigravity` | Antigravity CLI 1.5.2 consumer，Go Linux x86_64；ALPN 列表为空。 |
| `codex-http` | Codex 0.167.0 HTTP，Linux OpenSSL；ALPN 列表为空，不代表 macOS／Windows 指纹。 |
| `codex-ws` | Codex 0.167.0 WS，Linux rustls；扩展顺序为随机，ALPN 列表为空。 |

这些是 shape 数据的来源标签，不等于插件身份版本或本次验收结果；所有内置数据当前 `ws.over_h2` 均为 false。支持显式扩展顺序或 `random`；HRR、续接所需动态扩展不能当作静态列表中必然出现。部分套件只作声明，服务端选择 rustls 未实现的套件时握手失败，不伪装成功或降低验证；TLS 继续使用平台信任。TCP/IP、时序、QUIC/HTTP3、GREASE、ECH、ALPS 不在此形态对齐范围。连接和 TLS 会话按 shape 修订、代理及目的地隔离；本地捕获与测试仍须验证实际 wire 和具体代理／协议组合。

## 已确认的操作调度与后台执行限制

- 推理、完整搜索、媒体生成、OAuth、模型同步、额度查询与凭据刷新均由宿主创建操作并调用插件，不开放插件独立常驻后台任务。
- 宿主统一决定周期查询、凭据刷新时机，以及插件更新或停用期间的任务暂停和取消。
- 插件可在宿主发起的操作内部编排网络请求、处理流并并发执行必要的辅助调用；所有子任务受该操作的权限、取消和资源生命周期约束。
- 操作结束或取消后，不得遗留继续请求网络或写入存储的插件后台任务。
- 取消或 deadline 可以产生终态错误通知，但不能继续发布成功内容或写回数据。错误通知仍受插件代际撤销约束，慢消费者不能阻塞驱动释放资源。纯协议选择中的副作用、越权 origin 和非法 canonical 输出属于插件执行失败，不归责为客户端参数错误。
- 插件更新引起的取消与 deadline 在搜索、媒体及相应审计中保留强类型归责，不因为调用方 token 未取消就改报上游失败，也不通过取消调用方的全局 token 冒充同一事实。
- 需要跨操作复用的连接池或 WebSocket 由宿主管理，并保持连接与凭据隔离；连接复用不授予插件脱离操作生命周期执行的权力。
- 新的周期性供应商任务需通过明确的宿主能力接入，不允许借私有状态或初始化入口启动无人管理的定时任务。

## 已确认的网络白名单原则

- 插件必须在元数据中声明所需网络访问范围，宿主按白名单策略执行受控 HTTP / WebSocket 请求；未获白名单允许的目标默认拒绝。
- 网络访问范围不得仅隐含在插件代码中，也不得通过直接 socket 或重定向绕过白名单检查。
- 元数据声明是网络权限的输入，不表示插件可自行修改生效白名单。
- 允许访问的目标范围为当前连接由管理员保存的 base URL 所对应的 origin，以及插件声明并获准的 OAuth、模型发现、额度查询等附属服务 origin；不授予任意出站权限。
- 附属 origin 可由插件元数据声明为连接配置中的地址字段，例如 SAP AI Core 的 `tokenUrl`。宿主仅从管理员保存的值解析精确 origin，在保存或变更时展示范围，并将权限固定到操作快照；插件运行时返回值、私有状态和上游响应不能增加授权。
- 插件固定声明的 `OriginDeclaration.host` 支持原有精确主机字符串，或 `{"regex":"..."}` 形式的主机正则；精确主机仍不支持 `*` 通配。SDK 验证正则原始语法，宿主在冻结操作权限时编译完整主机匹配，非法表达式使整个权限策略构造失败。管理面明确展示正则及其协议、端口；该能力不适用于运行时返回的地址或连接字段值。
- origin 按协议、主机与有效端口匹配，路径由插件构造。HTTP 与 WebSocket 使用各自明确获准的协议，不因主机相同就隐式扩大协议或端口范围。
- 描述符明确声明非秘密布尔字段 `use_websocket`，且管理员保存为 `true` 时，宿主只为已保存 base URL 增加同主机、同有效端口的 `http→ws` / `https→wss` origin。未声明、关闭或缺省该字段均不扩张授权；附属地址、子域和其他端口不因此获准。
- 管理员可以显式将当前连接配置为本机或局域网服务，不一刀切禁止私网目标；这一配置不授权访问其他本机或私网地址。
- 安装或更新页面展示插件声明的附属服务地址。HTTP / WebSocket 首次请求、每次重定向及池化 WebSocket 的实际地址均检查同一冻结策略，不自动将原目标的认证头转发到另一 origin。
- 所有入口的连接管理与报文分帧仍由宿主负责；槽位、Content-Length 一致性、压缩声明与 WS 握手字段遵循上文 Net 1.x 契约。没有旧网络入口或隐式默认头回退，任何选项都不扩大 origin、凭据或代理权限。
- 使用出站代理不扩大允许访问的目标范围。允许访问目标与向目标发送凭据是不同的授权，不得根据网络白名单自动注入其他目标的凭据。
- 内嵌 `base` 随宿主自动更新时，新增声明的网络 origin 无需单独确认，由宿主随受信内置版本更新生效白名单；这不授予插件在运行期间自行修改声明或访问任意目标的权力。
- 本地包手动更新在已有确认界面展示网络范围变化，沿用一次更新确认，不额外增加网络授权步骤。

## 已确认的本地包导入方式

- 管理员在插件管理页面选择本机文件，导入当前连接的 Stravia 实例。
- Desktop 将所选文件导入本机实例；Server WebUI 将浏览器所在机器的文件上传并导入服务器实例，不要求浏览器与服务端位于同一机器。
- 不支持输入远程 URL 下载插件，不自动获取依赖；首期不增加输入服务器文件路径的安装入口。

## 已确认的标准 codec 依赖

- OpenAI-compatible（包括 embeddings）、Anthropic、Gemini、Open Responses 四类标准 codec 由 `stravia-protocol-codec` 共享维护，供应商插件不复制源码维护平行实现。
- 需要标准 codec 的 guest crate 在 Rust 源码层直接链接该库并锁定依赖，构建时静态编入最终交付的一个自包含 Wasm Component；不采用运行时宿主 codec，也不再把 Component composition 保留为未决定路径。
- Command Code 与 Devin 的私有 codec 分别由 `stravia-vendor-command-code` 与 `stravia-vendor-devin` 所有；Bedrock、Cohere、Gateway、WatsonX 等私有 codec 由 `stravia-vendor-base` 所有。`stravia-protocol-codec` 与宿主构建不得反向依赖这些供应商私有实现。
- 安装时无需另行获取 codec 包，运行时不由宿主替换插件内部 codec。每个插件产物携带的 codec 保持固定；随宿主升级自动更新内嵌的 `base` 时，替换的是整个插件产物及其锁定依赖，宿主契约兼容性仍需独立校验。专属插件的 codec 更新随其本地包更新交付。
- codec 修复通过重新构建、发布并更新相关插件交付，接受不同插件产物包含重复 codec 代码的代价，见 [ADR-0069](../adr/0069-bundle-pinned-codecs-in-vendor-plugins.md)。

## 已确认的本地包手动更新流程

1. 管理员选择本地插件包；宿主校验包结构、Vendor 身份及宿主契约兼容性。
2. 页面展示旧版本与新版本、受影响的 Provider，以及新代码将继承这些连接的凭据访问权。包自报的名称、作者或版本不能作为来源已验证的证据；来源未验证时如实说明。
3. 管理员点击“确认更新”，即授权新包接替旧版本并继承受影响连接的凭据访问权，不另设批准步骤。
4. 兼容更新成功后，新调用使用新版本，已开始的调用继续使用原版本，不在流中途替换实现；不兼容更新按下述规则取消该 Vendor 的全部活跃任务。
5. 校验或准备加载失败时保留旧版本可用，不先覆盖旧版本再尝试加载新包。

普通更新确认不自动授权丢弃既有数据；发现数据不兼容时，必须明确展示影响并由管理员确认是否丢弃，遵循下述不兼容更新规则。

## 已确认的内置插件自动更新

- 自动更新仅适用于程序默认内嵌且安装来源仍为内置的 `base`：随附版本更高，或版本相同但组件 SHA-256 摘要不同时，启动时自动更新，无需管理员确认；这是本地包手动确认规则的明确例外。相同版本的旧组件即使已无法加载或缺少产物，也按此规则替换为当前随附组件。
- `base` 的随附版本较低时不自动降级；版本与摘要都相同时不重复安装。
- 自动更新使用程序随附的 `base` 本地插件产物，不引入插件市场、远程包获取或依赖下载。
- 自动更新同样需要校验与准备加载；失败不先破坏原插件产物，也不代表不兼容的旧插件仍可在新宿主上执行。
- 插件管理页面展示自动更新后的实际版本与结果。该例外不扩展到专属插件；它们没有随附版本，只能通过本地包更新。
- 自动更新可以同时增加 `base` 声明的网络 origin，无需额外确认；管理员对内置更新来源的信任包含其网络声明变化。内置 `base` 的不兼容数据按格式版本差异选择性重置，也无需手动确认；本地包仍遵循手动确认规则。

## 已确认的本地替换与恢复内置

- 管理员通过本地包替换内嵌 `base` 的实现后，`base` 转为手动管理，不再被宿主随附的更高版本或同版本不同摘要组件自动覆盖。
- 自动更新资格由宿主记录的实际安装来源决定，不采信包自报的“官方”身份，也不因 Vendor ID 相同而恢复。
- 插件管理页面为 `base` 提供“恢复内置版本”操作，展示当前版本、程序随附版本及受影响连接；管理员确认后，使用随附实现并恢复随宿主自动更新。
- 内置来源的 `base` 加载失败或与随附版本不同时，也提供同一恢复入口，无需先导入本地替代包；已有待确认更新时，保留单一的更新审阅入口。
- 恢复内置可能替换为较低版本，必须明确展示；这一显式操作不改变内置自动更新只升级、不降级的规则。数据不兼容时同样需要丢弃数据的明确确认。
- 专属插件没有程序随附版本，不能执行“恢复内置”或随宿主自动升级；已经安装的版本及其数据不会因默认内嵌集合缩减而自动删除。

## 已确认的不兼容更新原则

- 允许更新为不兼容现有连接数据的插件版本，不将数据向后兼容作为绝对更新门槛。
- 本地包更新或手动恢复内置版本发现数据不兼容时，由管理员明确确认是否允许丢弃现有数据；普通更新确认不自动授权丢弃。
- 内置来源的 `base` 自动更新到更高随附版本或同版本不同摘要组件时，自动重置由旧、新 Provider Profile 的 `data_compat` 格式版本差异确定的不兼容数据，不等待管理员确认。自动更新不因插件程序版本或摘要变化就无差别清空数据。
- 只允许重置预览确定的不兼容插件数据，范围与任务协调规则如下；本地包和手动恢复仍要求明确确认。
- 数据不兼容与宿主 Interface 不兼容必须区分：允许丢弃数据不能让宿主执行其无法支持的插件契约。

## 已确认的数据丢弃范围

| 数据 | 处理规则 |
| --- | --- |
| 不兼容的供应商选项与插件私有状态 | 按格式版本差异选择性重置。 |
| 不兼容的上游凭据 | 选择性清除，要求重新登录或填写。 |
| 不兼容的模型发现元数据 | 清除后重新发现，恢复前不得用不兼容元数据执行。 |
| Provider 的 ID、名称与兼容的连接配置 | 保留，不因数据重置删除整个连接。 |
| Route / Target 绑定 | 保留，不自动删除或改绑；受影响 Target 在必要数据恢复前暂不可用。 |
| 历史、请求记录、已确认用量与额度历史采样 | 保留，不因插件更新删除。 |

本地包及手动恢复的确认界面必须列明哪些连接会丢弃哪些数据及恢复动作，不使用笼统的“清空插件数据”代替影响说明。内置自动更新按同一预览确定重置范围，并记录受影响连接及数据类别；其他未受影响连接与兼容的数据不在丢弃范围内。

## 已确认的不兼容更新任务协调

- 开始不兼容版本切换时，立即停止该 Vendor 的新任务准入，并取消该 Vendor 的全部活跃任务，不等待业务自然完成。
- 取消范围覆盖该 Vendor 的所有连接及待配置认证会话，包括推理、完整搜索、媒体生成、OAuth 登录与交换、凭据刷新、模型同步、额度查询及插件辅助调用；不仅限于需要丢弃数据的连接。其他 Vendor 不受影响。旧搜索报告与图片结果不能在取消后迟到提交。
- 本地包及手动恢复的确认界面同时说明数据丢弃范围和全部活跃任务将被中断的影响，不能只提示数据清理。
- 取消后必须确保旧版本任务不能继续执行或写回数据，再实施已确认的数据重置与版本切换；旧认证会话的迟到回调不得恢复已取消会话或写入凭据。等待取消清理不等于等待业务自然完成。
- 宿主取消不能保证上游已经停止执行或停止计费；保留已确认用量，不伪造未知用量，也不自动重放被中断的请求。
- 新版本启用后，具备有效配置与凭据的连接可以恢复接入；必要数据已清除的连接继续保持暂不可用，直到管理员重新配置或登录。
- 若无法确保旧任务已经停止且不能写回，不得继续丢弃数据或宣称切换成功。该取消例外不改变兼容更新允许在途调用使用旧版本完成的规则。

## 供应商真实对齐辅助工具

推理验证使用真实准入的 Generation Chain 根，并与正式 Model Turn 共用 `session_affinity` 的派生：按 Principal、Target 和链路根隔离，不采用客户端自报的会话标识。缺少此上下文会使依赖会话头的插件产生不完整请求，不能将这种验证器失败归因于产品传输栈。

`tests/common/vendor_alignment.py` 默认只读取来源目录的 SQLite 快照并输出清单，不发送网络请求；SQLite 使用只读模式、显式事务和 WAL，不迁移或复制生产库。使用 `--vendor` 选择组件，可重复；`--component` 指定单个外部 WASM 时必须同时指定一个 Vendor。默认发行组件来自 `target/vendor-plugins-all/manifest.json`，校验组件摘要，需先通过现有构建流程生成。

真实调用必须显式加 `--allow-live`，并通过 `--runner` 或 `STRAVIA_ALIGNMENT_RUNNER` 指定预先构建的 Core 单元测试可执行文件；`--scenario` 可重复选择 `infer`、`media-image`、`discover`、`allowance`，`--model` 固定请求模型，`--channel` 只选择指定渠道，没有匹配连接时明确失败。报告记录模型 ID、metadata 的 ID 是否匹配，以及按来源连接稳定查询顺序生成的本轮序号；序号不包含账号标识，也不能用于追认没有连接标识的历史调用。未指定推理模型时仍取保存目录的排序首项，不代表官方默认模型；需要同模型对照时必须显式指定。来源目录无当前用户读取权限时可加 `--sudo-reader`：只有只读 SQLite 子进程提权，Core 执行进程不提权。`--output` 必须指向源码树和来源数据目录以外的路径。输出不保存凭据或原始响应；隔离的新用户单轮上下文并非来源账户原有会话，请求一致性尚未比较时明确报告 `not_compared`，不把调用成功当作官方客户端一致性证明。

运行器先用迁移后的全新内存 SQLite 和受限临时目录启动并排空后台任务，再装入选定连接，不传递 refresh token，不改真实到期时间；已知过期的 OAuth 不发请求，未知期限保持未知并由上游判定。每次操作只执行一次，不对凭据拒绝自动刷新重试。无源连接时，显式选择 `opencode-free` 可匿名发现；匿名推理还需指定模型，运行器先从真实发现结果验证其免费资格。其他缺少凭据的插件报告缺失，不伪造成功。仅对获授权且已记录摘要的组件运行，沿用 Core 的插件声明网络策略和重定向凭据防护，不由脚本向其他 origin 复制凭据。

未指定生图模型时，运行器先调用同一组件的真实模型发现，只选择明确声明图片输出能力的模型；没有可确认模型则报告 `image_model_unavailable`，不猜测型号。此时报告的请求模型仍为 `null`，不能据此恢复内部发现选中的 ID。错误响应诊断仅在测试构建的内存中保留最多 256 KiB，并对 gzip、deflate、br、zstd 做同样有界的解码；未知、损坏、截断或超限正文保持未知。报告保留已有的 `model_error_kind`、`retry_after_ms`，并以 `http_observation` 单独记录实际收到的 HTTP 响应数量、最后状态及明确的订阅耗尽头判定；它不把 SSE 业务状态冒充 HTTP 状态，也不预读 HTTP 200 的正文。正文报告只含格式、字段存在性及固定错误类别，不输出正文或未知自由错误码。HTTP 400／403／429、模型列表成功和本地合成捕获都不等于真实推理对齐成功。

外部插件 wrapper 可通过 `--response-code-map PATH` 提供测试专用的数字错误码映射：JSON 为 `{"fields":["code"],"codes":{"42":"rate_limit"}}`，文件最多 16 KiB、最多 64 个码；fields 只能选择 `code`、`type`、`status`、`error`，码键必须为规范的 i64 十进制字符串，类别只能来自报告的固定闭集。共享脚本和 Core 运行器分别校验，配置经既有私有输入通道传递；映射不改变生产错误分类、认证或重试，也不增加 WIT 导出。
