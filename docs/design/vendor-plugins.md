# Vendor Wasm 插件

所有模型 Vendor 通过同一套 Wasm Component 执行。Core 拥有客户端协议、权限、调度、历史和持久化；插件拥有所选供应商的上游流程及编解码。

本文说明运行时契约；来源认证、安装、更新、恢复和卸载见[插件市场与分发](plugin-marketplace.md)。

## Vendor 身份与信任边界

### 执行身份与目录身份

一个插件提供一个稳定的 `vendor_id`，不由 npm、目录或协议命名决定。
`base` 是唯一 `fallback` Vendor，可声明多个不同 Provider Profile；`dedicated` 必须恰有一个 Profile，且 `provider_id` 等于 `vendor_id`。
`provider_id` 决定执行 Profile，`catalog_id` 只关联目录；两者都不是已保存连接的 UUID。见 [ADR-0067](../adr/0067-separate-vendor-identity-from-package-and-protocol.md) 与 [ADR-0070](../adr/0070-run-all-model-vendors-as-wasm-plugins.md)。

专属 Profile 整体拥有其身份下的全部连接、channel 和操作。缺包、加载失败、能力不支持或执行失败时明确失败，**不得回退到 `base`**，也不合并声明、权限或实现。
普通 OpenAI、Anthropic 与 xAI API Profile 由 `base` 提供，与 Codex、Claude Code 和 Grok 的订阅连接分开。

新建连接选择器按「目录身份 + channel」隐藏被外部插件替代的对应 Base 入口；其他 channel 与不同外部候选仍可选。
完整描述符查询仍用于编辑已有连接。替代仅影响新建候选，不改写存量连接的 Profile 或执行归属。

选择器请求 `GET /api/v1/vendors?selectable=true`，管理已有连接则读完整 `GET /api/v1/vendors`；前者隐藏匹配入口不影响后者。
候选交给 Core 的是实际所选 `provider_id` 与 channel，不拿目录 ID 路由。

### 凭据授权与健康

管理员在所选插件的连接中登录或填写秘密，即授权该组件处理**这一连接**的上游凭据。
宿主持久化与隔离连接凭据；组件看不到其他连接、管理会话或客户端 API Key，也不能将网络 origin 声明当作访问其他凭据的许可。见 [ADR-0068](../adr/0068-trust-vendor-plugins-with-connection-credentials.md)。

- Core 统一刷新 OAuth。非认证操作冻结连接快照时，具备 refresh grant 且临近到期（60 秒内）、最近刷新失败或已有刷新租约时，等待共享交换；未知到期时间不补 TTL。
- 同一 Provider、OAuth 连接和配置代际共享交换，跨进程使用存储 CAS。请求取消只停止自己的等待；关闭、换连接或撤销组件代际仍可阻止交换写回。
- 临时刷新失败保留既有凭据与身份，记录刷新错误。上游未返回新的非空 refresh token 时保留旧值。
- 上游 401 仅在未提交、同一组件／连接代际、deadline 和宿主恢复预算内尝试一次刷新后重发；403 不触发此恢复。分页与后续发送沿用新凭据，不退回已拒绝的快照。
- OAuth 刷新状态 `error` 不是 Credential Invalid。只有明确授权拒绝、无法恢复的已拒绝凭据或刷新后仍拒绝才持久化标记 `credential_status = invalid`，依据 [ADR-0073（凭据失效）](../adr/0073-mark-provider-credential-invalid-on-upstream-auth-rejection.md)。
- 失效连接从路由资格移除，后台任务跳过，不靠半开探测解封。显式重新连接、有效凭据验证或更换凭据按版本条件恢复；已撤销 grant 须重新登录。不修改 `is_enabled`。
- OAuth 续接与压缩绑定稳定认证连接身份，正常刷新不改变它；重新登录产生新身份。非 OAuth 秘密变更会改变凭据指纹。

共享刷新有独立时限，进程间回收租约与交换的完成是两件事；等待方超时不能把旧凭据冒充刷新的结果。
正常刷新推进健康版本并不意味着刚做的连接检查无效；保存或提交时按实际执行快照重新核验配置、身份和 fence。

### 连接检查

`POST /api/v1/providers/{id}/test` 无配置草稿时检查已保存连接；带草稿时冻结本次地址、选项、凭据、代理和 staged OAuth。
空密钥沿用同一已保存连接；草稿不保存配置、不绑定 OAuth、不写连接测试或健康状态。动态目录 Profile 也必须按有效描述符分发。

宿主先校验结构及 `ConfigValidation`，再调用必需的 `ConnectionCheck`。没有安全只读探针应显式返回中性 `unsupported`；静态成功或配置有效不等于连通。

- 读取真实 HTTP 状态与协议结构，不执行生成、模型同步、onboarding、AssignModel 或私有状态写入。
- 成功只报告已证实的 `endpoint`、`credentials` 或 `endpoint_and_credentials`。公开目录、鉴权语义未知的探针或仅发送认证头不能证明凭据有效。
- 已保存的显式检查可用于停用连接。只有凭据验证成功才可恢复健康；写回绑定实际使用的 Provider／OAuth 版本和 publication fence，不能覆盖并发更改。
- 失败区分配置、传输、上游拒绝与响应解析，界面用本地化 code，不展示原始上游正文。耗时是整次检查时长，不是推理延迟。

### 能力与模型发现

同一连接可声明普通推理、完整搜索、媒体生成及独立的额度读取，也可仅有其中一项。按实际 Profile、channel 和 Provider Model 校验，不能从同包其他 Profile 推定。
推理、完整搜索与生图都通过 Provider Model → Target → Route，专用模型不冒充聊天模型。完整搜索不是 Local Web Provider；独立执行与报告见[联网搜索](web-search.md)，图片能力与适配器见[媒体生成](media-generation.md#route-与-provider-接入)。

- `base` 用上次成功目录快照启动，无快照用内嵌清单；后台成功刷新整体切换，失败保留当前版本。只注册能映射到已实现认证和协议的 Profile。
- 删除目录条目后禁止用它新建连接，保留存量连接执行；使用已删除 Catalog scope 的模型同步明确报告 `ProviderNotFound`。
- 模型集合由显式配置、账号发现或 channel 声明的目录策略决定。账号发现失败不伪装为目录成功；Core Canonical Model 仅可富化已经确定的成员，不扩展集合。
- `catalog_id` 映射到供应商 scope。仅 channel 声明消费目录时使用该 scope，不从旧创建入口或连接名猜策略。明确的发现地址保留完整路径、查询和本 Profile 的认证方式。
- 最近成功来源与人工规则的合并见[逐字段固定](cost-and-billing.md#逐字段固定与恢复同步)。全局 Canonical 目录和 Base Provider 目录独立刷新，具体系统归属见[架构](architecture.md)。

设置显式静态模型时只决定允许的集合；不会扩大已认证账号的权限。可选供应商目录故障时可用 Canonical 元数据补已确定成员，仍要报告目录失败并保留已有有效值。
元数据匹配歧义不得猜测，也不从目录重新分派另一 Profile 或扩大网络权限。

### Allowance Monitor 与额度守护

额度 Monitor 由宿主调度，读取所选 Profile 的正式额度端点并按连接缓存、采样。
额度不是本地估算账本；缺字段、未知单位或查询错误不是零，也不等于耗尽。快照中的计费建议见[成本与计费](cost-and-billing.md#provider-成本方式)。

- 管理员只可守护账户级条目。新的成功读取显示任一受守护条目耗尽（已用 ≥ 100% 或剩余 ≤ 0）时，持久化暂停该 Provider 所有 Route 上的 Target。
- 成功读到全部守护条目可用，或移除相应守护，才解除暂停。达到 `reset_at` 只触发强制读取；失败、条目缺失或单次推理 `QuotaExceeded` 不直接改变暂停。
- 证据必须更新，并绑定凭据版本、连接和组件；配置、凭据或结果过时不能写回新身份。守护独立于启用状态和 Credential Invalid。
- 保存守护先提交本地配置再安排合并后台读取。移除守护可立即收窄暂停；新增守护不以缓存判耗尽。所有候选都仅因额度暂停不可用时返回额度型 429，不承诺恢复时间。

见 [ADR-0078](../adr/0078-suspend-provider-routing-on-guarded-allowance-exhaustion.md)。

### 一次性重置权益

Codex 与 Claude Code 可报告账号的一次性重置权益；不能从额度窗口、套餐名或残缺数据推算权益或次数。只有管理员确认选定权益才执行，不后台消费或购买。

- Codex 依上游 `available_count` 显示次数，只允许已确认的个人账户权益；普通额度窗口不决定能否兑换。
- Claude Code 只取 `cedar_ember` 的逐份权益，在同一 OAuth 账户的组织下兑换；不混入另一计划。
- 提交前读取 fresh 证据，按连接串行并绑定权益、凭据、组件和请求 ID；消费不经自动认证恢复重放。只有上游业务结果明确为 `reset` 才成功，后置额度读取失败单独提示。
- 结果不明时用户重试须复用同一请求 ID；未决记录仅在当前进程，重启后未知旧请求不提交上游，先去上游核对。重置不直接解除守护暂停。

## WIT 契约与受控出站网络

### Wasmtime 准入与编解码

唯一执行机制为 Wasmtime + WebAssembly Component Model + WIT。
[Vendor WIT](../../backend/crates/stravia-vendor-sdk/wit/vendor.wit) 描述组件导出；独立 [stravia-net](../../backend/crates/stravia-vendor-sdk/wit/deps/stravia-net/net.wit) 与 [stravia-state](../../backend/crates/stravia-vendor-sdk/wit/deps/stravia-state/state.wit) 描述宿主资源。
使用 [Rust SDK](../../backend/crates/stravia-vendor-sdk/src)、[Wasmtime runtime](../../backend/crates/stravia-vendor-runtime) 与[已发布快照](../../backend/crates/stravia-vendor-sdk/wit/released)查具体签名和字段，不跨 ABI 暴露 Rust trait、数据库或 Gateway 对象。

- Vendor 稳定 1.0.x、Net／State 已知接口至 1.1.x；拒绝其他主版本、预发布和未知接口，同主版本的更新 minor 要求更新宿主。准入仍须 Linker 验证实际函数、类型和资源。
- minor 仅增加独立可选接口或开放键值能力；不改变已有封闭类型、签名和必需导出。旧稳定 1.0 组件仍可加载；无 0.x 兼容入口。见 [ADR-0093](../adr/0093-split-controlled-network-into-stravia-net-with-major-version-admission.md)。
- 受限 WASI 只允许已有的 stdio、terminal、空 environment、exit、clocks、random 和 io 支持接口；不继承宿主环境、目录或 stdio，拒绝 filesystem、sockets、WASI HTTP。
- 每次操作独立 Store、资源表与 HostServices，复用的编译产物不共享权限或运行状态。等待实例资源、CPU 执行及 I/O 均受取消和 deadline 约束。见 [ADR-0072](../adr/0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)。
- 四类标准 codec 由 [stravia-protocol-codec](../../backend/crates/stravia-protocol-codec) 维护，锁定依赖并静态编入自包含组件；私有 codec 留在所属 guest。宿主升级不替换独立组件内的 codec。见 [ADR-0069](../adr/0069-bundle-pinned-codecs-in-vendor-plugins.md)。

### 网络授权与传输

插件通过 Net 资源主动编排 HTTP／WebSocket、OAuth、发现及辅助调用；宿主执行实际网络、代理、限制、取消和诊断。
宿主以 rama 和打补丁的 rustls 执行连接和传输，不给 guest 裸 socket 或任意代理开关。见 [ADR-0071](../adr/0071-orchestrate-vendors-through-host-network-capabilities.md) 与 [ADR-0094](../adr/0094-own-vendor-egress-with-rama-and-patched-rustls.md)。

- 冻结权限只来自连接的已保存 base URL，及描述符声明、获准的固定附属 origin 或管理员保存的附属地址字段。按协议、主机和有效端口精确区分；允许声明的固定 host 正则必须全主机匹配、在管理面可见。
- 只有声明非秘密布尔 `use_websocket` 且管理员确实开启时，才为 base URL 增加同主机、同有效端口对应的 ws/wss origin；不扩大附属地址权限。
- 用户可将连接明确配置为本机或私网服务，只允许该 origin。上游响应、OAuth 返回地址、重定向、私有状态和代理不能扩大授权。
- 检查首次请求、每跳重定向和池化连接实际地址；独立判定能否发送凭据，不自动把原 origin 的认证转给另一 origin。
- 有序字段保留能在目标协议表达的顺序、大小写、重复值和字节；HTTP/2 仍遵循协议限制。不会自动注入 Accept、Accept-Encoding 或 User-Agent。
- Slot Header 是交给宿主按实际连接或正文补值的位置，而不是插件预设的任意 header。Host、分帧及 WS 握手值由宿主校验；冲突、非法值或未知选项在出站前报错。
- 压缩、解码与握手约束以 [Net SDK](../../backend/crates/stravia-vendor-sdk/src/net) 为准。压缩声明须与字段一致，不把未知编码、畸形握手或无效选项静默降级。
- 可选 Cookie Jar 只选声明的域和 cookie 名，不增加 origin 权限；按安装、Vendor、Provider、数据代际和 fence 隔离。内存状态重启清空；草稿检查使用单操作独立 jar，不继承或写回连接 cookie。

Net 错误码是开放字符串；未知错误和未知传输原因不授权重放。选择 HTTP/1、HTTP/2 或 WS 不给组件新的 origin 权限。
有效 slot 只能按协议填补实际值，不能用来提供不一致的 Host、长度或几个冲突的 framing 字段。
宿主代理和认证范围以冻结操作为准；连接复用不能跨 Provider、凭据、组件或其他已绑定的 transport 身份。

### Egress Shape

Egress Shape 是宿主校验执行的连接层数据：TLS ClientHello、ALPN、HTTP/1 槽位、HTTP/2 帧与伪头顺序和 WS 传输。
不包含请求头值、URL、凭据、额外信任或重试策略；有效形态不证明账号资格，也不等于对端接受请求。见 [ADR-0095](../adr/0095-describe-client-egress-as-host-shapes.md)。

- 内置 shape 用裸名，见[数据](../../backend/crates/stravia-core/src/plugin/network/shape/data)；组件声明用 `plugin:<local-name>`，仅在自身注册表解析，不能覆盖或引用其他组件形态。
- 安装准入及启动恢复时编译并验证；组件与 registry 一次发布。操作及认证固定取得的快照，不混用旧组件与新 shape。
- 池与 TLS 会话按组件、shape 修订、代理和目的地等事实隔离。形态不改变证书验证器；无法实际支持的套件应握手失败，不降低验证。

形态声明不是另一个客户端头列表或自动默认 header 集。
组件可以同时声明不同的 HTTP 和 WS shape，但只有宿主真正支持并执行的传输原语有效。

### 调度、写回与发布

所有推理、搜索、媒体、认证、发现、额度和刷新由宿主建立操作。插件只能在操作内编排网络、流与受控子任务，不运行独立后台任务。
普通推理默认共享 300 秒无活动期限，有边界活动可续期；显式固定 deadline、首 Token 限制和取消仍各自适用。

- 同一 Model Turn 的恢复与重发固定原组件、操作租约和数据代际，不在重试中切换到管理器新版本。插件只分类失败，不能自行重放模型生成。
- 宿主按结果提交边界决定重试和 failover；有效输出前有界暂存控制事件，超限明确失败，不丢事件后继续。操作结束后不留下网络、存储或发布任务。
- Vendor Write Permit 同时持有操作准入和写回围栏，直到结果或事务提交；取消、代际撤销及多步写回的临界区必须按当前身份复检。
- 发布围栏还检查调用方取消和 deadline。迟到 Provider 状态、凭据、模型、工具报告或图片不得在权限已撤销后成功提交；失败通知也不得阻塞资源归还。
- 纯协议选择禁止网络、私有状态与事件副作用；权限越界和非法 canonical 输出属于插件执行失败。可无副作用返回固定协议的 channel 可声明 `static_protocol`，由宿主在正常准入后短路。

安装排空与任务中断流程见[市场生命周期](plugin-marketplace.md#数据兼容与任务协调)，写回约束以[生命周期实现](../../backend/crates/stravia-core/src/plugin/lifecycle.rs)为准。

## 自定义协议与思考等级映射

### 私有协议与 Custom

插件只注册上游协议实现，不注册客户端 HTTP 路由、认证或 MCP 工具。私有协议保留开放的 `ProtocolIdentity`，不冒充共享协议枚举。
编码器按 canonical 语义向目标正向投影；不能表达的内容省略，不伪造空参数、用量或签名。转换边界见 [Open Responses](open-responses.md#表达能力与省略规则)。未知私有媒体载体接收 canonical 内联内容，不误用签名 URL。

`base` 仅注册一个 `custom` Profile，枚举 OpenAI-compatible、Open Responses、Anthropic Messages 和 Gemini；不合并专属插件的私有协议，管理面也不接受自报的任意协议字符串。见 [ADR-0074](../adr/0074-select-custom-protocols-through-one-base-profile.md)。

### 客户端头与请求控制

Channel 通过 `client_header_names` 明确声明额外可读头：合法小写、精确名称，无通配符。只赋给本次选中的 Profile/channel；不声明就不提供额外头。
拒绝凭据、代理、Cookie、Host 和 hop-by-hop／framing 头，不传非法控制字节。预置 channel 不声明额外头。

已知多协议请求控制先归一为一个 canonical 值，再按目标白名单投影。
不从 raw request、旧别名或通用 extra-body 恢复冲突值；省略未支持控制不递归裁剪合法业务 metadata、工具 JSON Schema 或参数内部键。
会话、用户轮、窗口、父节点等来自宿主证明，不能用客户端 header、Options 或 canonical 自报标签覆盖。

### Responses 传输与续接

Custom Responses 的 `use_websocket`（默认关闭）和 `use_previous_response_id`（默认开启）相互独立。HTTP/SSE 为常规传输，开启 WS 则使用同一授权 URL 的 ws/wss 对应地址；独立压缩仍走 HTTP。
WS 可复用连接且不用上游 ID。关闭上游 ID 续接只关闭优化，不关闭 Stravia 客户端续接、本地历史、受保护推理恢复或 prompt cache。

- 续接必须证明原账号、Target、协议、有效历史与控制等价；Custom WS 须原子取到精确 tip 的可用 socket，即使 `store=true` 也不能推定代理上游已保存响应。
- socket 不可用时不发送增量；成功完成且报告上游 ID 才保留 tip，取消、中途断流或未知终态销毁 socket。连接寿命从握手起算，复用不续期。
- 插件一次 Infer 最多实际发送一次生成。连接本地续接无法发送，或上游明确尚未执行并拒绝续接时，宿主在 Client Output Commit 前可用完整历史恢复一次，不耗 Target Retry Budget；重新通过发送准入，不重置总 deadline。
- 普通错误、已提交输出或执行状态不明不归此恢复。WS 特有回退还受正常同 Target 重试预算约束；恢复保留真实错误与已确认用量，不造一次额外客户端失败。
- HTTP 按真实上游存储事实判断。完整历史恢复时按当前 Principal 物化原媒体，不发送内部 Artifact 引用；客户端协议与原生压缩规则见 [Open Responses](open-responses.md#原生压缩)。

Custom Chat 的 `reasoning_field` 可选协议默认、`reasoning_content` 或 `reasoning`，同时控制上游响应读取和历史回放；非字符串非 null 报错，不猜其他别名。
独立的 `ensure_reasoning_field` 只在本次上游 assistant 历史中把缺失或 null 补为 `""`，不覆盖真实文本、不改历史、不改变思考强度。其他协议不应用这两个选项。

Responses 标准 codec 将出站 system message 原位写为 developer；消息顺序与独立 instructions 不变，不把该兼容规则做成连接开关。
只对实际送往目标的视图转换，不回写客户端历史，也不补回客户端自己丢弃的续接历史。

### Thinking Level Map

Target 的 Thinking Level Map 仅包含两类目标控制：映射到上游等级（`Effort`）或关闭此等级（`Hidden`）。Generated 来自有效 `reasoning_efforts` 规格；Overridden 由管理员指定；Hidden 不参与选择。声明了 `reasoning_efforts` 的模型以声明集合为严格白名单；开关型与旧手动预算型号在发现阶段发布规范 effort 集合，并在出口编解码器／插件中翻译为对应上游开关或静态预算。
规范档位未精确命中时先选最近的更高可用档，没有更高再向下。不要把某供应商私有示例做成全局档位清单。

- **Default Thinking Level** 是 Route 上仅在客户端完全没给推理控制（且未显式重置推理参数）时生效的默认档；显式客户端控制或重置优先。
- **Supported Thinking Levels** 是启用 Target 的非隐藏映射并集，供展示和客户端配置导出，不是运行时强制钳制集合。
- 发现、修改或恢复同步使有效 `reasoning_efforts` 变化时，同事务重算 Generated 行，保留人工覆盖和隐藏。最终映射由所选目标 codec／插件在出口按型号能力编码（Anthropic 新旧代自适应／静态预算与 `max_tokens` 约束、Gemini 3+ `thinkingLevel` 与 2.5 `thinkingBudget` 互斥、开关型供应商翻译等）。
- 合法受保护思考先试一次；明确拒绝时由宿主只剥离外来或来源不明的载荷并受控重放，不移除本上游载荷。见 [ADR-0080](../adr/0080-try-protected-reasoning-once-and-block-only-foreign-origins.md) 与 [ADR-0100](../adr/0100-simplify-thinking-level-map.md)。

## 动态表单、认证与私有状态

### 表单与多语言

插件声明字段、类型、分组、默认值与简单条件，宿主统一呈现，不执行 guest 提供的 HTML、JavaScript 或组件。
通用约束由宿主校验，跨字段业务规则由 guest 校验；普通选项与秘密分离。`visible_when` 可引用字段或保留的 `$protocol`，隐藏字段不绕过与其相同条件的必填语义。

- 派生地址只是建议；显式地址不被默认值覆盖。最终地址与精确 origin 经管理员审阅确认后才能保存；草稿变化使旧确认失效，OAuth 不以未审阅地址授权。
- OAuth 链接可复制或打开同一已建立会话；由外部浏览器办理。宿主校验 callback 和 state；若 guest 有特定 state 格式，可返回合规替换值，仍由宿主验证。
- 插件作者维护 `messages/en-US.json` 与 `messages/zh-CN.json`；[SDK 构建生成器](../../backend/crates/stravia-vendor-sdk/build-support/messages.rs)校验稳定 key 和具名参数，翻译编入组件，不加载远端语言包。
- 描述符和校验响应携带纯文本 `LocalizedText`，非空 `en-US` 必填，仓库维护的产品文案同时提供中文。界面精确 locale 匹配，缺失回退英语；不按供应商补译，也不翻译原始上游错误。
- 语言变化不改变字段 key、枚举 value、分组、协议、错误 code、草稿、焦点或地址确认，不重发插件校验。无效描述符拒绝发布并保留 last-good；翻译变化本身不升级配置数据格式。

### Profile 图标

组件声明的自包含 SVG 只作宿主图像响应和页面图像显示，不注入 DOM。缺内嵌图标时，宿主依次查目录 logo 和获准的网站 favicon，不携带 Provider 凭据。
目录 logo 优先按 `catalog_id`，未映射才用 `provider_id`；绝不以连接 UUID 查目录。官网未声明才取连接 origin，不从文档地址猜官网。
远端图标由实例缓存，24 小时到期后按需更新，失败先复用该来源旧缓存。无可用图像再使用通用表示；WebUI 不直接访问第三方图标。

### Vendor Private State 与 Vendor Conversation State

Private State 给当前 Profile 与 Provider 连接保存设备注册或账号绑定等必要数据；未保存连接只用对应认证会话临时状态。
WIT 整体读换，单作用域最多 256 KiB；超限不改旧值。声明数据格式版本，不开放任意 SQL、文件、其他连接或绕过专用凭据契约的入口。

`stravia-state` 提供按版本 CAS 的 cell。Provider cell 仍受 Vendor／连接限制；Conversation State 另按 Principal 与获准 lineage 隔离，guest 不能自选主体或分支。
协议选择、目录同步及无会话准入的操作不能访问 Conversation State。缺失和删除的 revision 不等价，CAS 冲突只能重新读取并计算，不能重发上游请求。

- Conversation journal 把不可变内容引用与 cell 的新 head 原子发布；append 自身不是发布，也不证明上游执行。checkpoint、fork、压缩与失败 admission 继承已证明的快照，不复制大段累计正文。
- 历史边界证明绑定真实持久 canonical 视图；codec 将可信 gap 映射成 native offset，不按重复文本、客户端标签或剩余消息猜位置。缺失、篡改、跨分支或歧义证据不得采用。
- 虚拟安装身份跨重启、Target 与令牌刷新稳定，不采集真实硬件；用户输入、逻辑轮、模型轮和上下文窗口是不同身份。工具续接与同轮重试不重复计数；缺少必要旧事实时保持未知或明确失败。
- volatile cell 与 Cookie Jar 都是进程内资源，不用于恢复持久会话；代际撤销使旧 handle 失效。状态写回遵守 fence，网络 I/O 不持有数据库事务。

State 资源的 key、payload 和 journal 正文有明确配额；超限失败，不截断、降级进程内复用或改写原 head。
不会为拿到历史的旧分支授权 sibling 的 checkpoint。来源资格丢失后，晚到操作不发布；清理仅回收无有效引用的正文。

## 专属 Vendor 的接入边界

配置、身份版本及逐字段 wire 以相应 crate 为准；私有订阅协议不承诺账号资格、模型可用性或未来兼容。客户端形态一致不等于实际运行官方客户端，也不改变上游条款。

### Codex · `openai-codex`

[Codex 插件](../../backend/crates/stravia-vendor-codex)使用订阅 Responses wire，推理与独立压缩固定流式；非流式客户端由宿主汇总。独立压缩走末尾带 trigger 的流式 Responses，不调用公共 `/responses/compact`。

- 官方模型 metadata 决定 Responses Lite 或普通形态；Lite 的前缀及原生偏移由会话 journal 和可信 gap 投影，WS 增量不重复前缀。
- thread、逻辑轮、window 和 installation 由宿主证明提供。HTTP 不发 WS-only 续接 ID；若拿到仅有增量的输入，先向宿主报告不可用，不删 ID 后照发增量。
- Fast 按连接配置，仅覆盖普通 HTTP/WS 推理的服务档位，不改变发现、额度、搜索、生图或独立压缩；官方 hosted 搜索和生图使用各自操作，见[媒体生成](media-generation.md)。
- 额度 credits 保留原单位和小数，不换算成 USD；重置按上文权益规则。

### Claude Code · `claude-code`

[Claude Code 插件](../../backend/crates/stravia-vendor-claudecode)使用订阅 OAuth 与交互式 Messages 流；保留调用者 system 与工具，不复制 CLI 工作区、权限上下文或全部公共 API 参数。只有所属版本的真实 wire 能证明相应能力，参见 [Fable 5.2 捕获](../research/claude-code-fable-5-2.md)。

- 支持的模型在有账号身份和可信会话时使用 Message Threads；符合严格追加及固定参数等价、上次成功未过期才 continue，否则发送全量 create。
- 线程锚点只存联系 ID、摘要、计数与时间。明确的执行前线程拒绝由宿主在安全边界内恢复一次；普通限流、传输失败或未知执行状态不属于此路径。
- 自定义工具名称隔离同时作用于声明和历史回放，不改变业务 schema 或工具 ID。上游不能承载的 strict 语义、prefill 与 truncation 控制不伪装成支持。
- 不声明或执行 Anthropic Remote Compaction，也不将普通消息摘要当成其替代；非压缩思考／工具清理仍按其协议。见 [ADR-0098](../adr/0098-remove-anthropic-remote-compaction.md)。

### Antigravity · `antigravity`

[Antigravity 插件](../../backend/crates/stravia-vendor-antigravity)用网站回调后的手动授权码登录，不依赖 loopback；项目接入未完成应报告 pending，保留账号证据，不猜 project 或 tier。参见 [OAuth 捕获](../research/antigravity-oauth.md)。

- 推理目录只选登录账号 Agent 列表，依据真实重定向、家族和档位生成目标 selector；不按显示名或型号 enum 盲合并。显式档位须有真实映射，不构造模型名或降档。见[模型映射](../research/antigravity-model-mapping.md)。
- `system_prompt_prefix` 默认开启；仅改变本次上游推理副本的身份前缀与顶层指令位置，不改本地历史，也不作用于搜索、生图、发现或额度。
- 会话累计计数基于宿主持久 Model Turn 事实与 CAS，分支独立；遇到历史缺口或跨 Provider 不补造步数。任何归属信息都不能当作上游已执行的证明。
- 完整搜索用已登记的搜索身份，不猜其他工具或降级文本答案，不提供外部续接。协议证据见[搜索捕获](../research/antigravity-cli-web-search.md)；生图映射见[媒体生成](media-generation.md)。

### Kiro · `kiro`

[Kiro 插件](../../backend/crates/stravia-vendor-kiro)支持 Google、GitHub、Builder ID 和 IAM Identity Center 设备授权；IAM 目录 region 与实际 Kiro profile region 不必相同。推理、模型和额度按账号 profile 路由，不读 CLI 登录文件。

模型与强度来自账号目录。只有原生 schema 同时准许 adaptive 和 disabled 才补充对应关闭映射；未知档位不猜。未映射通用控制省略，工具业务 schema 保留，不加载 CLI 内置工具。
月度、试用、赠送分开呈现，不合成总余额。参见[请求对齐](../research/kiro-cli-request-alignment.md)、[额度](../research/kiro-cli-credits.md)与 [TLS 捕获](../research/kiro-cli-tls.md)。

### Grok · `xai-grok`

[Grok 插件](../../backend/crates/stravia-vendor-grok)使用 Grok 专属 OAuth Responses，普通 xAI API 属于 Base。会话标识来自宿主作用域，每次请求另有独立请求 ID；不接受客户端自报身份。参见 [Grok OAuth](../research/grok-oauth.md)。

### Command Code · `command-code`

[Command Code 插件](../../backend/crates/stravia-vendor-command-code)以账号订阅筛选客户端模型目录；无有效订阅、未知套餐、取数失败或无候选明确报错。
静态配置只可缩小授权集合。发现的 `off` 在统一规格记为 `none`，私有编码再投回 `off`。
NDJSON 无原生响应 ID，插件在有效输出前生成 canonical MessageStart，宿主绑定真正的 Generation 节点。

### Devin · `devin`

[Devin 插件](../../backend/crates/stravia-vendor-devin)拥有 Connect-RPC／protobuf、模型发现与 AssignModel，保持 cascade、step 与 JWT 的原生绑定。
会话绑定保存在 Provider CAS；缺亲和信息使用一次性身份，不从 token 或客户端标签猜复用，也不为了清空容量重放请求。
思考晚封口不改变开始时顺序。若额度窗口仍报告有效重置时间，省略的 proto3 零剩余不丢掉耗尽窗口；两项皆无不造窗口。

### OpenCode Free · `opencode-free`

[OpenCode Free 插件](../../backend/crates/stravia-vendor-opencode-free)使用独立免费模型入口，按可信会话与独立用户消息身份构造请求，不把目录可读当作推理许可。
只补必要的工具兼容声明，不附带工具执行器、不伪造结果。保留上游拒绝，不轮换出口、盲重试或偷偷换模型；账号条款与证据范围见 [OpenCode Free 捕获](../research/opencode-free-models.md)。
