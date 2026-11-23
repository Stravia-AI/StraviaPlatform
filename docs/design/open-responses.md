# Open Responses 2026-04-24

Stravia 以官方 **Open Responses 2026-04-24** 为唯一 Responses 协议基线，提供 HTTP JSON、SSE 和 WebSocket。它不是 OpenAI rolling Responses API，也不承诺复制该产品的服务端语义。协议独立定版见 [ADR-0011](../adr/0011-own-open-responses-as-a-dated-protocol.md)，共享转换与 HookRuntime 见[系统架构](architecture.md)。

## 协议身份与规范

| 层级 | 身份 |
| --- | --- |
| Rust | `Protocol::OpenResponses` / `OPEN_RESPONSES_2026_04_24` |
| 配置与 CLI | `open-responses` |
| Wire ID | `open-responses/responses/2026-04-24` |
| HTTP / WebSocket | `/v1/responses` |

解释冲突的顺序：RFC 2119 normative Specification → 日期固定的 OpenAPI → 同日期 Reference → 官方 acceptance 可观察行为。请求只接受 JSON，不因 Reference 或 OpenAPI 列出 form 而接受表单。

Ingress 接受已建模、结构安全的 rolling additive surface，但这不改变定版身份。额外字段不自动获得 canonical 语义或任意透传权限。
标准之外的已注册扩展使用 `<owner-canonical-slug>:<name>`，Stravia 自有扩展固定为 `stravia:*`；注册由 adapter 静态拥有。未知 namespaced 值不会作为已知语义发送。
平台工具使用标准 function 声明（例如 [StraviaRead](agent-core.md#straviaread-统一读取契约)），不把内部执行过程编码成私有进度事件。公开 `stravia:agent_result` 使用标准 item 生命周期，结果由 `path` 引用。

## Canonical Item 图

请求、响应、续接和转换围绕有序 `AiItem` 图。图保留条目身份、生命周期、来源与可见性；message 按 role、phase 和 content part 保序，调用与结果只按 `call_id` 关联。
正文、拒答、媒体、annotation、思考和 tool arguments 使用有类型的语义载体。用量与 typed error 属于响应／流事实，不凭缺失字段补造。
工具结果的字符串、原生 content-array 和显式业务 JSON 保留不同语义。业务数组不因形似媒体块而转换为 content-array。

### 指令与推理

顶层 `instructions` 是独立控制，不等于 System 或 Developer message。Responses 出站将 System message 编为 `developer`；不改 canonical role，不合并或移动条目，也不复制到顶层 `instructions`。
其它协议有等价 Developer role 时保留，无该角色时按兼容例外投影为 System。

- 原生 Responses reasoning 保存 summary、content 与真实 `encrypted_content`。通用 Thinking 的可读段落可作为客户端 reasoning content，别的协议的签名不是 Responses 密文。
- 跨协议思考的公开预览与对应 History Marker 用同一 reasoning carrier；按 [ADR-0088](../adr/0088-carry-responses-thinking-markers-in-encrypted-content.md)，令牌由 `encrypted_content` 承载，不是上游加密载荷。回放只恢复当前 Principal 可验证的原始项，不转发未知令牌或重复注入预览。
- 上游 Responses **请求**中的原生 reasoning 保留可用的 summary 和密文，`content` 清空；可读段落按原顺序投影到随后的 assistant `output_text`。拆分产生的载体不借用源 item 的公开 ID、状态或 phase，不改写真实客户端输出历史。

### 流式语义

Canonical 状态表达 response、item、part、delta/done、usage 与错误；SSE `event`、`output_index`、`content_index`、`sequence_number`、WebSocket framing 与 `[DONE]` 属于 wire session。
Responses reasoning content 使用 rolling `response.reasoning_text.delta` / `.done` 输出名称，同时接受 dated `response.reasoning.delta` / `.done`；summary 生命周期独立。
同步与流式最终公开投影必须一致，不能把 wire index 当永久身份或按原始接收顺序重建不同的客户端历史。

## HTTP 与 ResponseResource

- `POST /v1/responses` 接受唯一、非空的 `Authorization: Bearer <token>`；scheme 不区分大小写，不以 `x-api-key` 等请求头替代。
- 接受 `application/json` 及合法 media type 参数；不支持的媒体类型返回 `415 unsupported_media_type`。
- 请求解压后上限为 100 MiB。支持 gzip / deflate / br / zstd；不支持的编码返回 415，损坏压缩数据返回 400，超限返回 413。
- 响应按 `Accept-Encoding` 选择压缩，等权重优先 zstd；SSE、图片、音视频和短响应不使用该 HTTP 压缩。
- 格式或语义错误在模型发送前拒绝。普通根请求需有 `model` 与 `input`；续接可省略 `input` 并继承父模型，`generate:false` 见[本地预热](#本地预热与不支持的能力)。

ResponseResource 由 Gateway 管理公开 ID、时间与 lifecycle；`model` 是逻辑 Route ID，不是 Provider Model ID。返回 dated schema 所需字段，不添加顶层 `output_text` 便捷字段。
展示 effective defaults 与可继承的请求配置分别保存：上游可回报实际执行值，但不因此改变下一请求的配置。只有明确解析的父请求控制可继承。

| 省略时的响应展示 | 默认值 |
| --- | --- |
| `temperature` / `top_p` | `1` |
| `presence_penalty` / `frequency_penalty` / `top_logprobs` | `0` |
| `parallel_tool_calls` | `true` |
| `truncation` / `service_tier` / `tool_choice` | `disabled` / `default` / `auto` |
| `text` / `tools` | `{"format":{"type":"text"}}` / `[]` |
| `metadata` / `store` / `background` | `{}` / `false` / `false` |

合法标准上游明确回报的执行值可用于实际展示；Gateway ID、逻辑模型、客户端的 `store`、`metadata`、`safety_identifier` 与本地生命周期仍由 Gateway 决定。
同步 JSON 与终态 snapshot 共用用量投影：总量、cached tokens 和 reasoning tokens 都可信才返回完整 `usage`；否则整体为 `null`，显式零不丢失。
`text.format.description` 在请求中出现时必须是字符串，符合响应 schema 的 `null` 不作为后续请求参数。

## SSE 与 WebSocket

### SSE

- `event:` 必须等于 JSON `type`，不发送 SSE `id:`；最终事件的 `sequence_number` 由 Gateway 重新分配，严格递增且无重复。
- 顺序为 response → item → part → delta/done → terminal；终态之后发送 `[DONE]`，它不是事件也不占 sequence。
- 普通 delta 和 added 即时交付。`response.output_item.done` 按最终 `output_index` 收口；内容、身份与顺序须等于终态 `response.output`，不因上游交错或稀疏索引而破坏历史。
- 原生 reasoning 可等待权威 ItemDone 收集晚到的保护载荷；不是缓冲整条流。失败与 incomplete 也排空应完成的 item，且每项只完成一次。

### WebSocket

复用 `/v1/responses`，每轮输入一个 `response.create`。一条连接可顺序执行多轮，同一时刻最多一个在途响应；并发 create 得到 WS 409 `response_in_progress`，不打断在途请求。

| 约束 | 行为 |
| --- | --- |
| 认证 | 握手使用 Bearer；开始每轮重验当前 Key、权限与用量准入，不从消息体取 token |
| Origin | 有 Origin 时校验 Proxy CORS allowlist；无 Origin 的 CLI/SDK 可以连接 |
| 消息大小 | 解压后单消息 100 MiB |
| 帧压缩 | 仅协商成功的 `permessage-deflate`，遵守双方 window bits 与 context takeover 参数 |
| 交付 | 有界出站队列背压，不丢事件；连接断开即取消在途 Run |
| 连接寿命 | 最长 60 分钟；单个 Run 另受请求 deadline 约束 |

WS 文本帧承载事件 JSON，不使用 SSE 的字段、分帧或 `[DONE]`。下游 WS 与 Vendor 上游传输独立；下游 HTTP 并不要求上游使用 HTTP。

### 终态与错误

普通响应已经开始后失败，交付 `response.failed`，SSE 随后发送 `[DONE]`。不在失败快照前发送会提前截断读取的独立 `error`，不伪造 `response.completed`。
失败快照保留部分正文、思考、工具和上游已报告的用量；未确认完成的输出项标为 `incomplete`，不用 response 级 `failed` 填写 item 状态。

未提交的失败走 HTTP 错误或相应 WS 错误；已开始的响应保证失败快照使用 dated 嵌套 `error`。传输中断、超时和临时不可用可归类为 `server_error`，认证、配额、永久请求错误和本地 Hook/投影错误不混作瞬时失败。
结构安全的 additive 字段与真正的 schema、序列或生命周期违例不同；协议违例显式失败，不猜测修复。公开错误不回显内部诊断，也不增加重试权限。独立 Compact 的上游原生错误保留例外见[原生压缩](#原生压缩)。

模型正常拒答不同于上游错误：有结构化拒答时使用 `completed` 中的 `refusal` part，不因缺少普通正文重试。上游账户侧状态不进入拒答内容。

### 客户端错误投影

| 条件 | 对客结果 |
| --- | --- |
| 非法 JSON、请求 schema 或语义 | `400 invalid_request` |
| Bearer 缺失或无效 | 401 |
| Origin 不在允许范围 | 403 或拒绝 WS upgrade |
| 申请的父响应或 item 不可见 | `400 previous_response_not_found` / `item_reference_not_found` |
| 当前 WS 已有 response | WS 409 `response_in_progress` |

HTTP 状态和 WebSocket 上报的 `status` 表达同一个原因。已开始的流以 response 终态表达失败，不再承诺新的 HTTP 状态。通用认证与 RPM、额度和错误归属由[路由](architecture.md#路由与访问控制)与[交互观察](interaction-observation.md)负责。

## Response Chain 与 store

Response Chain 是 [Generation Chain](data-layout-performance.md#generation-chain-物化与提交) 的 Responses 投影。客户端的 `previous_response_id` 只引用 Stravia 公开 ID，不接受上游 ID 作为公共身份。历史按 Principal 隔离，允许多个后继分别从同一父节点分支。

| 本轮 `store` | 本地历史 | Responses 上游 |
| --- | --- | --- |
| `true` | 正常提交条件不变 | 允许 Provider 存储，实际存储仍需确认 |
| 缺省或 `false` | 同样保留完整已交付历史，HTTP/WS 断开后仍可本地续接 | 发送 `false`；临时上游 ID 不能作为跨连接锚点 |

`store` 是本轮的 Upstream Store Hint，不控制 Stravia 历史，不从父请求继承。上游明确回报未存储时，不以客户端的 `store:true` 推断已持久化。
`metadata` 与 `safety_identifier` 属于本地公开资源，不发往远端 Provider。

### 显式父响应与条目引用

显式 `previous_response_id` 优先，展开逻辑为 `parent input → parent output → new input`。
只继承父请求的已解析控制；本轮显式值优先，显式清空阻止继承，不从父响应展示值推算工具、思考或采样控制。
不存在、被回收或跨 Principal 的父 ID 统一返回 `previous_response_not_found`；关联写入仍在进行时只等待该写入，不越过真实提交。

Item reference 接受 `{ "id": "…" }` 或带 `type: "item_reference"` 的等价形式。先查当前请求图，再查当前 WS connection-local 图，然后查该 Principal 可见的本地链。
找不到时返回 `400 item_reference_not_found`；其它主体的存在性不外泄。祖先引用目录不能用压缩后的最终执行窗口代替。

### Target Continuation

本地历史作为完整事实源；上游 ID 只优化目标发送。资格校验包括同 Principal、精确 Target、账号/连接与配置代次、resolved model、egress、控制和规范化后的完整模型视图。
脱敏、思考控制或上下文有语义变化时使用完整模型窗口，不把客户端 Marker 当成上游原生状态。

- HTTP 续接需要已确认的上游存储事实；仅有临时 ID 不够。
- 临时 WS 续接必须原子取得与所需 response ID 匹配的同 socket 最新成功 tip。分支占用、tip 前移、淘汰和断线均在发送前回退完整窗口，不消耗恢复次数。
- 真正发送过增量请求后，只有上游明确拒绝该续接且尚无模型响应事件时才允许一次完整窗口重发，仍受准入和取消约束；未知接受状态、普通 400、已提交输出或再次失败不适用。普通重试门禁见[路由与访问控制](architecture.md#路由与访问控制)。

### 上游传输与会话

OpenAI direct 与 Codex OAuth 的生成可走 Vendor 上游 Responses WebSocket；连接身份和隔离由 [Vendor 插件](vendor-plugins.md) 的受控出站契约裁决。普通客户端传输与 stream 选项不要求复制到上游传输。

自定义 Open Responses 连接的 `use_websocket` 默认关闭，`use_previous_response_id` 默认开启，两者互相独立。
关闭上游 ID 优化只改变目标发送，不关闭本地历史、客户端续接、媒体和思考恢复或 prompt cache。

重发完整历史时，使用当前 Principal 和已选协议重新物化有权访问的媒体引用。只改写即将发送的副本，不把内部 Artifact URI 发给上游，也不预读正常增量路径不需要的祖先媒体。

### Reusable Response Prefix

对提交完整历史的请求，可在已完成 Hook、目标语义准备及表达能力投影后，查同一 Target 的最长严格有效前缀。Hook 仍观察完整逻辑窗口。
仅在完整 `AiItem` 边界比较，正文、思考载荷、工具关联、角色、媒体及控制均参与，不做模糊文本匹配。
候选必须已完整交付、上游为 `completed`、存在上游 ID 且响应 Hook 未改写输出；按匹配长度、完成时间和节点 ID 确定顺序。
不满足资格或整个请求完全相等时不生成空自动增量；未命中发送完整窗口。

### 本地预热与不支持的能力

`generate:false` 允许空或省略输入，仍校验 Principal、父响应、Route 与根准入；返回空 `output` 的 completed 响应，完整交付后写本地链。
不执行 Hook、模型、Target 或工具，不提供上游锚点，也不把预热输入记为已通过 Request Hook；该控制不继承，不与独立压缩组合。
`background:true` 返回 `400 unsupported_feature`；不提供 retrieve、cancel、轮询或 OpenAI conversation 资源。

## 表达能力与省略规则

目标有等价表示时必须转换（例如正文、支持的媒体、函数调用与结果关联、目标可承载的 JSON Schema 输出）。
跨协议转换只省略目标确实无法表达的语义：内容、扩展、控制、硬约束、item 身份和候选数都可能受限。该规则对请求、完整响应、流增量与续接历史一致，不提供严格模式。

- 混合消息逐项处理，不能因一项不支持删除其可表达的同项正文；同协议可承载字段不随意丢弃。
- 对只接受对象参数的目标，无法表达的 tool call 与关联结果一起省略，不伪造 `{}`；需要缓冲时按调用收集，不阻塞无关正文、思考或用量。
- 保留已声明的 `strict:true/false`。非 Responses 入口的工具未设置 `strict` 而目标为 Responses 时编码 `strict:false`，避免目标把原本可选参数变必填。
- 普通客户端工具参数由客户端解析，未修改的原始字符串不因格式欠缺而被修复。凭据保护和目标实际对象要求仍独立生效；平台工具在实际执行边界解析参数。
- 不支持的 `truncation` 或约束不会被改写成提示词、伪造状态或自动裁剪历史。非法源输入、schema 错误、安全／凭据失败、真实协议违例和传输错误仍走错误路径。

### 工具、媒体与输出控制

Gemini 目标以 `responseJsonSchema` 表达输出 schema，以 `parametersJsonSchema` 表达工具参数；保留目标可表达的本地 `$defs` 与 `$ref`，不内联递归引用或访问外部 schema URI。
工具选择映射为 `AUTO`、`NONE` 或 `ANY`；指定名称时保留 `allowedFunctionNames`。目标实际支持的子集仍由上游裁决。

Anthropic 目标使用原生 `output_config.format` 表达可支持的 schema；不能通过把开放对象改为 `additionalProperties: false` 假装转换成功。
Gemini 工具结果要求对象时，标量或数组业务值由 `{"result": …}` 承载；Anthropic 业务 JSON 可使用协议合法的文本载体，不猜测数组中的元素类型。

媒体按合法原生载体逐项编码。支持的 PDF 或纯文本可使用文件或 inline data；标准 Messages 不制造音频 block。无法表达的附件不会被 stringify 后冒充等价输入，也不因此删除同消息的正文。

### Gemini 3 工具历史导入

只有官方 Gemini HTTPS `generativelanguage.googleapis.com` 的 `/v1beta/models/…:generateContent` / `:streamGenerateContent` 和 Antigravity 默认原生端点可在已确认的 Gemini 3 / 3.x 公开模型上使用 `skip_thought_signature_validator`。
当前用户轮次内，逐 model step 检查首个函数调用：仅当它的签名缺失、为 `null` 或为空时补该请求控制；已有非空签名不改，也不处理其后的并行调用。
该值只写目标 wire，不存入 canonical 历史、不代表原生证明；Vertex、非官方地址、代理自定义 endpoint 或不明模型别名不适用。

## 原生压缩

[ADR-0098](../adr/0098-remove-anthropic-remote-compaction.md) 规定独立原生压缩只支持 **Responses V2**：`POST /v1/responses` 或 `response.create`，采用流式契约，`input` 末尾恰好一个 `compaction_trigger`。
位置或数量非法返回 `invalid_request`；旧 `/v1/responses/compact` 不提供路由。独立压缩经过正常选路、认证和发送准入，不创建 Generation，也不运行 Agent 循环。

已选 Target 须支持 Responses Compact；不支持则返回 `compaction_unsupported`，不会寻找其他 Target，不重试、不转成普通推理，也不在本地生成摘要。
目标使用标准流式 `/responses`、`store=false`；保留已支持的工具与推理控制，仅当没有显式 `instructions` 时补入交接摘要指令。实际上游错误保留原生 envelope，本地失败仍使用公开屏蔽规则。

客户端收到标准流及**恰好一个**已完成的原生 compaction item；由客户端决定保留窗口，不合成 `response.compaction` 资源。
`response.output_item.done` 的完整条目可作为权威状态，即使后续 Codex `response.completed.output` 不重复；若终态给出相同索引，两者必须一致。截断、拒绝、重复状态、未完成输出都不登记为成功。

### Inline 控制

普通生成可携带 `context_management` 原生压缩阈值。仅在客户端控制缺省时，支持的 Route 自动策略才可补阈值；实际是否触发由所选 Responses 上游决定。
Stravia 不按本地 Token 统计主动压缩，不把未触发的阈值伪装成压缩事件。已完成原生状态使用同一登记和确认规则，不推迟无关正文。

### 来源与执行窗口

[ADR-0047](../adr/0047-separate-native-compaction-ancestry-from-context.md) 分开保存**压缩来源**和**实际执行窗口**。完整原生状态在可回放字节交付前登记，真实交付或合法引用确认后才记录确认事实。
状态绑定 Principal、精确 Target、Provider 账号/配置代次、模型与协议；公开 Generation ID、上游 response ID、原生 compaction 身份互不替换。

恢复优先识别当前状态的已确认严格后继，未匹配才关联登记的来源；冲突引用不猜测。控件变化仍可关联能验证祖先的已交付后继，但不借此继承旧控件、窗口或上游 ID。
压缩边界保存 Replace；冷恢复或 Target Continuation 失效时也只发送当前压缩窗口，不按父边重新拼回来源全文。保留与回收见[数据布局](data-layout-performance.md#共享正文存储与引用回收)。

Anthropic 与 Claude Code 远程压缩已移除：Messages 不接受压缩控制、compact edit 或历史 compaction 块；向其投影明确压缩状态也返回错误。
不回放旧 Anthropic 签名，不包装跨协议密文，不提供纯文本降级。其它普通推理的 inline 阈值按目标表达能力处理，但省略阈值不能绕过已登记状态的来源和绑定校验。

`supports_remote_compaction` 是只读派生能力；至少配置一个 Target，且全部已配置 Target（包括禁用）均选 Open Responses 并通过实际 Compact 准入才为真。

## Schema 合规入口

仓库锁定 [2026-04-24 OpenAPI JSON](../../backend/crates/stravia-protocol-codec/tests/fixtures/open_responses_2026_04_24.openapi.json)，保留[来源与摘要](../../backend/crates/stravia-core/tests/fixtures/open_responses_2026_04_24.source.json)。
生产使用 typed codec，不在运行时加载或解释此文件；[Schema 验证入口](../../backend/crates/stravia-protocol-codec/src/codec/open_responses/schema_tests.rs) 将固定 artifact 作为协议断言依据。兼容客户端的 additive 能力不等于完整 OpenAI rolling conformance。
