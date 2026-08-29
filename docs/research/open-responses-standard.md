# Open Responses（OpenResponses）开放标准一手研究

| 项 | 值 |
| --- | --- |
| 研究截点 | 2026-08-14 |
| 官方名称 | **Open Responses**（项目仓库为 `openresponses/openresponses`）；本文将其简称为 OpenResponses。 |
| 结论 | 它是受 OpenAI Responses API 启发、面向多供应商的开放规范，不是 OpenAI Responses API 的别名或 OpenAI 的产品文档。Stravia 应将其作为独立 wire contract，并把 OpenAI 当作一个 adapter。 |
| 来源限定 | 仅使用 OpenResponses 官方站点/仓库、OpenAPI/schema、官方变更记录及 OpenAI 官方 API 参考；易变事实均附一手链接。 |

## 1. 执行摘要

1. **身份与治理：** Open Responses 自称 vendor-neutral、community-governed 的 LLM API specification；其范围包括规范、reference implementations、conformance tests、文档和 tooling。治理由 Contributors、Maintainers、Core Maintainers、Lead Core Maintainer 和 Technical Steering Committee（TSC）组成，不为任何公司保留席位，单一 vendor 不得控制 Core Maintainer 多数。[S1][S2]
2. **截至截点的最新已发布规范：** `2026-04-24`（发布日期/版本日期 2026-04-24）；官方 changelog 没有更新的 dated release。对应 OpenAPI JSON 与 Reference 均有版本化 URL。仓库 `main` 截至截点可识别的最新提交是 `92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c`（2026-07-14 21:55:16Z，合并 “Version the specification and reference”），因此报告以该 commit 的 schema tree 和 `2026-04-24` release artifacts 为基线，而不把 moving `main` 当作新版本。[S3][S4][S5]
3. **覆盖范围：** 核心是 `/v1/responses` 的 request/response、items、semantic SSE events、函数工具调用、文本/图像/文件输入、reasoning 和状态机；2026-04-24 另加同一 resource 上的 WebSocket transport 与 `/v1/responses/compact`。[S6][S7]
4. **Stravia 最低面：** 实现 Bearer auth、`POST /v1/responses` JSON 同步响应、`stream=true` 的 SSE、`response.*` 生命周期和 `response.output_text.*`，`message` + `function_call`/`function_call_output` 闭环，`input_text`/`input_image`/`input_file`，`previous_response_id`（或明确无状态拒绝），以及结构化 HTTP/stream error。WebSocket、compaction、provider-hosted tools 可作为 capability-gated extension。
5. **主要风险：** OpenResponses 正文使用 RFC 2119 文字规定请求体 **MUST** 为 `application/json`，但其 Reference 页面和生成的 OpenAPI path 又列出 `application/x-www-form-urlencoded`；二者是官方材料内部冲突。Stravia 应按规范正文的 MUST 执行（仅接受 JSON），并把该冲突记录为待 TSC/规范澄清，而非默默支持两种媒体类型。[S6][S8]

## 2. 标准身份、官网、仓库、schema 与版本治理

### 2.1 标准是谁

官网将 Open Responses 定义为 “open-source specification and ecosystem” 以及用于构建 multi-provider、interoperable LLM interfaces 的 shared schema/tooling layer；其目标是跨 provider 描述请求、输出、streaming 和 agentic workflow。[S1]

Technical Charter 的 mission 是 “open, vendor-neutral API specification”，灵感来自并与 OpenAI `/v1/responses` interoperable；它明确把 Open Responses specification、reference implementations、conformance tests、documentation 和 supporting tooling 纳入项目 scope。[S2]

**边界：** OpenAI 官方 `Responses API` 是 OpenAI 的厂商 API；Open Responses 只是以其为 inspiration/interoperability target 的独立社区规范。OpenAI 文档出现的字段或 hosted tool，除非出现在 OpenResponses 版本化 schema 中，不应被称为“OpenResponses 标准字段”。[S1][S2][S9]

### 2.2 治理

Technical Charter 规定：

- Contributors 可提交 code、documentation、specification、issues 或其它技术 artifact；Maintainers 具有一个或多个仓库的 commit 权限；Core Maintainers 负责总体技术方向；Lead Core Maintainer 在无共识时作最终技术决定。
- TSC 由 Core Maintainers 与 Lead Core Maintainer 组成，负责规范、extensions/deprecations、reference implementations、tests、release/compatibility/versioning policy 和争议解决。
- 所有治理角色归个人而非组织；不为特定公司保留席位；单一 vendor 不得控制 Core Maintainer 多数；TSC meetings open。
- 规范/文档默认 CC-BY-4.0，代码 Apache-2.0，代码贡献需 DCO sign-off。[S2]

这说明它具有公开治理章程，但截至本研究未发现独立注册标准组织、IETF RFC 编号或正式的 semver major/minor 发行序列；当前官网采用 ISO 日期版本化 artifacts。实现时应 pin 日期版本和 schema hash，而不是写 `latest`。

### 2.3 版本与提交证据

官方 Changelog 只有两条 dated release：2026-01-15（项目 launch）和 2026-04-24（WebSocket、compaction、assistant `phase`、optional `logprobs` 等）。[S4]

`openresponses/openresponses` GitHub API 在截点返回的最新 `main` commit：

```text
92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c
2026-07-14T21:55:16Z
Merge pull request #77 ... Version the specification and reference
```

仓库的该 commit 包含 `schema/components/schemas/*.json`、`public/openapi/2026-01-15/openapi.json` 和 `public/openapi/2026-04-24/openapi.json`；这为版本化 schema 和 release artifact 提供可复核记录。[S3][S5]

**基线声明：** 本报告的“当前最新版”是官方 changelog 的最新 dated release `2026-04-24`；上述 2026-07-14 commit 是最新可识别源码快照，不是另一个已发布规范版本。

## 3. 传输、认证与端点

### 3.1 HTTP/SSE

规范正文要求 HTTP messages，`Authorization` 与 `Content-Type` header 必须存在；request body MUST 是 `application/json`。非流式 response body 只能是 `application/json`。流式 response 必须是 `Content-Type: text/event-stream`，每个 `data` 是 JSON-encoded string，terminal event 必须为 literal `[DONE]`；`event` field 必须匹配 event body 的 `type`，server SHOULD NOT 使用 `id`。[S6]

OpenAPI 2026-04-24 的 server URL 为 `https://api.openai.com/v1`，其相对路径为 `POST /responses`，部署时对应 `/v1/responses`；响应支持 `application/json` 和 `text/event-stream`，SSE union 列出了下文事件。[S7]

### 3.2 WebSocket（2026-04-24 新增）

服务器 MAY 在同一 `/v1/responses` resource 上暴露 WebSocket。客户端每个 turn 首条消息必须是 `{"type":"response.create", ...}`；除 HTTP/SSE 专属的 `stream`、`stream_options`、`background` 外，字段沿用 response creation request。服务端用与 HTTP SSE 相同的 event objects；一条连接最多一个 in-flight response，多个 `response.create` 必须顺序处理，不得 multiplex。官方还规定：

- `previous_response_id` continuation 可只发送新 input items；`store=false` 可用 connection-local state 继续，但无持久化 fallback 时必须以 `previous_response_not_found` 失败。
- continuation 4xx/5xx 后必须 evict 引用 response；连接最长 60 分钟，达到上限报 `websocket_connection_limit_reached`。
- WebSocket error 是含 `type: "error"`、`status` 与 `error.code` 的 JSON envelope。
- standalone `/responses/compact` 返回 compacted input window，而不是 response ID；之后新 turn 应省略/置空 `previous_response_id`。[S6][S4][S7]

### 3.3 端点清单（标准自身）

| 端点 | 版本状态 | 作用 |
| --- | --- | --- |
| `POST /v1/responses` | 2026-01-15 起；2026-04-24 增加 WebSocket transport metadata | 创建 response；同步 JSON 或 `stream=true` SSE；WS 使用同一 resource 的 `response.create`。 |
| `POST /v1/responses/compact` | 2026-04-24 新增 | 压缩 conversation/input，返回 `CompactResource`，用于后续新 response。 |

版本化 OpenAPI `paths` 仅声明这两个 HTTP path；不要把 OpenAI 其它资源端点（或 OpenAI hosted tool endpoint）误登记为 OpenResponses 端点。[S7]

## 4. 请求、响应与 item schema

### 4.1 Create response request

`CreateResponseBody` 的核心字段如下（均以 `2026-04-24` OpenAPI 为准）：

- `model`：模型标识；`input`：字符串或 input item array。
- `previous_response_id`：延续上一 response 链；`instructions`：额外指令；`store`：是否保存 response；`background`：后台执行提示。
- `tools`：工具定义（核心 `FunctionToolParam`）；`tool_choice`：`none`/`auto`/`required` 或指定 function；`parallel_tool_calls`；`max_tool_calls`。
- `text`：文本输出格式（plain text 或 JSON schema）；`reasoning`；`max_output_tokens`。
- `stream` 与 `stream_options`；`include`（当前 schema 枚举包括 `reasoning.encrypted_content`、`message.output_text.logprobs`）；`metadata`；`temperature`、`top_p`、`presence_penalty`、`frequency_penalty`、`top_logprobs`；`truncation`（`auto`/`disabled`）；`service_tier`。[S7][S8]

核心 item 是带 `type` discriminator 的 polymorphic object。标准正文规定 item 是 context 的 atomic unit，既可作为 input，也可作为 output；item 有 `in_progress`、`incomplete`、`completed` lifecycle。`incomplete` 是 terminal，必须是最后 item 且令 containing response 也 `incomplete`；`completed` 后不得再更新。[S6]

当前公共 item/content 轴：

| 方向 | 标准类型 | 关键字段/含义 |
| --- | --- | --- |
| 输入消息 | `message`、`UserMessageItemParam`/`SystemMessageItemParam`/`DeveloperMessageItemParam`/`AssistantMessageItemParam` | `role`、`content`、可选 `id`/`status`；assistant message 在 2026-04-24 可有 `phase`：`commentary` 或 `final_answer`。 |
| 输入内容 | `input_text` | `text`。 |
| 输入图像 | `input_image` | `image_url`（fully-qualified URL 或 data URL）与 `detail`：`low`/`high`/`auto`。 |
| 输入文件 | `input_file` | `filename`、`file_url`（参数 schema 也支持 `file_data`）。 |
| 模型输出 | `message` + `output_text`、`refusal` | 输出文本和拒答内容。 |
| 推理 | `reasoning` + `summary`/`content`/`encrypted_content` | raw reasoning 可选；encrypted form opaque；summary 可选。 |
| 开发者工具调用 | `function_call` | `name`、`call_id`、JSON-string `arguments`、lifecycle status。 |
| 工具结果 | `function_call_output` | `call_id`、`output`（字符串，或 text/image/file/video content array）、status；客户端执行 function 后在下一 turn 回传。 |
| 压缩结果 | `compaction` | `encrypted_content`；只在 2026-04-24 compact flow 出现。 |

标准正文允许 provider-specific item，但要求 type 使用 canonical provider slug 前缀（示例 `openai:web_search_call`）；这不是把 `openai:*` 变成 vendor-neutral core，而是一个明确的 extension escape hatch。[S6]

### 4.2 Response object

`ResponseResource` 的核心是 `id`、`object: "response"`、`created_at`、`completed_at`、`status`、`incomplete_details`、`model`、`previous_response_id`、`instructions`、`output`、`parallel_tool_calls`、`reasoning`、`store`、`background`、采样/text/tool 配置、`usage`、`metadata`、`service_tier` 和 `top_logprobs`。`output` 是 item array，`usage` 包括 `input_tokens`、`output_tokens`、`total_tokens` 及 input/output token details。[S7]

生命周期事件使用 `queued`、`in_progress`、`completed`、`failed`、`incomplete` 等 response status；item status 与 response status 是两个层级，不能把一次工具调用的 `completed` 直接等同为整个 response completed。[S6][S7]

## 5. Streaming events

事件是 semantic events 而非纯文本/object delta；有 response state transitions 和 item/content deltas 两类。首个 output item event 必须是 `response.output_item.added`；streamable content 先 `response.content_part.added`，再 delta，最后 content-specific done 与 `response.content_part.done`，item 最后 `response.output_item.done`。[S6]

2026-04-24 OpenAPI SSE union 的标准事件（事件名保留原文）为：

- response lifecycle：`response.created`、`response.queued`、`response.in_progress`、`response.completed`、`response.failed`、`response.incomplete`；
- item lifecycle：`response.output_item.added`、`response.output_item.done`；
- content/reasoning：`response.reasoning_summary_part.added`、`response.reasoning_summary_part.done`、`response.content_part.added`、`response.content_part.done`、`response.output_text.delta`、`response.output_text.done`、`response.refusal.delta`、`response.refusal.done`、`response.reasoning.delta`、`response.reasoning.done`、`response.reasoning_summary_text.delta`、`response.reasoning_summary_text.done`；
- annotation/function：`response.output_text.annotation.added`、`response.function_call_arguments.delta`、`response.function_call_arguments.done`；
- stream error：`error`；
- WS client event（非 server SSE）：`response.create`；WS failures 使用 error envelope，而不是新增 response item event。[S6][S7]

每个事件带 `type`、`sequence_number`，另按事件携带 `response`、`item`、`item_id`、`output_index`、`content_index`、`delta`、`text`、`arguments` 等。Stravia 必须保持 sequence ordering 和 add/delta/done 配对；不得只把 SSE 拼成一段字符串后丢弃 lifecycle。

## 6. 工具调用

标准正文将工具分为：

- **Externally-hosted tools：** 实现位于 provider 外；function 是典型例子，模型只生成 call，developer 执行并在第二个 request 以 `function_call_output` 回传。MCP 亦属于外部实现，但控制方式可不同。
- **Internally-hosted tools：** 在 provider 系统内执行，例如 OpenAI file search；Open Responses 定义 hosted-tool pattern，但没有把某一供应商的 hosted tool 名称都纳入 core。[S6]

`FunctionToolParam` 的标准公共面是 `type: "function"`、`name`、`description`、JSON Schema `parameters`、`strict`；`tool_choice` 可为 `none`、`auto`、`required` 或指定 function，且 schema 支持 `allowed_tools` 形式以限制可实际调用的 subset。[S7][S6]

**最低可互操作闭环：** gateway 接收 function schema → provider 输出 `function_call`（完整或增量 arguments）→客户端执行 → gateway 接收匹配 `call_id` 的 `function_call_output` →带 `previous_response_id` 或完整上下文继续。未知/不支持的 hosted tool 应返回可诊断的 `invalid_request`，不得静默转为 function。

## 7. 多模态覆盖

标准核心明确区分 UserContent 与 ModelContent：用户侧可包含 text、image 和 file 等输入；基线模型输出主要是 text，provider 未来可扩展 `output_image` 等 provider item。[S6]

2026-04-24 OpenAPI 的普通 user-message 输入 union 明确支持 `input_text`、`input_image`（URL/data URL 与 `detail`）、`input_file`（URL/file data/filename）。同一版本还通过 additive patch 定义 `InputVideoContent`（`type: "input_video"`、`video_url`），并将其加入返回态 `Message.content` 与 `function_call_output.output` content union，但没有加入 `UserMessageItemParam.content`。因此视频可作为 function tool output 内容回传，却不是与 image/file 对等的普通用户消息输入；实现和兼容声明必须区分这两个位置。[S7][S10]

标准没有定义音频 input/output、统一二进制 artifact URL 生命周期或文件上传 API。此处属于“未标准化/需 provider extension”。

## 8. 错误

标准 HTTP error 是结构化 error object，包含 `type`、可选 `code`、`param`、人类可读 `message`；正文示例是 `invalid_request_error` + `model_not_found`。规范列出的类别和建议状态包括 `server_error`/500、`invalid_request`/400、`not_found`/404、`model_error`/500、`too_many_requests`/429。流中用 `error` event，且发生 streaming error 后应有 `response.failed`；WS 使用 `{type:"error", status, error:{code,message,param}}`。[S6][S7]

**兼容注意：** 类别文字、code 枚举和 HTTP status 不是可任意互换的。网关应保留 upstream code/message/param，映射到自己的 error taxonomy 时保留原始值；遇到 provider-specific error 不应伪造标准 code。

## 9. 与 OpenAI Responses API 的逐项对照

OpenAI 官方 create reference 明确把 `/responses` 描述为可接受 text/image/file inputs，生成 text/JSON outputs，并可调用 custom code 或 built-in tools（如 web search/file search）。其当前请求字段还包括 `conversation`、`context_management`、`prompt`、`moderation`、`prompt_cache_options`、`prompt_cache_retention`、`user` 等。[S9]

| 主题 | 相同/可直接复用 | OpenResponses 与 OpenAI 的差异 | 尚未覆盖/兼容边界 |
| --- | --- | --- | --- |
| 核心 resource | `/v1/responses`、`model`、`input`、`instructions`、`previous_response_id`、response `id/status/output/usage` 语义高度重合。 | OpenResponses 以日期版本和公开 OpenAPI 固化；OpenAI API 是单一厂商的 rolling product contract。 | 不应因字段同名就假设未来版本仍 binary-compatible；pin `2026-04-24`。 |
| HTTP/SSE | JSON request、JSON response、`text/event-stream`、`[DONE]`、`response.created`/lifecycle、item/content delta 思路相同。 | OpenResponses 正文额外规范 `event` 与 `type` 一致、SHOULD NOT `id`；其 SSE union 是标准可互操作事件集合。 | OpenResponses 的 Reference/OpenAPI 还允许 `application/x-www-form-urlencoded`，与正文 MUST JSON 冲突；OpenAI create reference不应被用来消除该冲突。 |
| WebSocket | 都可围绕 response continuation 复用 response object（OpenAI 当前是否在所有部署开放 WS 不能由 OpenAI create reference 推断）。 | WebSocket 是 OpenResponses 2026-04-24 明确新增的可选同-resource transport，含 sequential turns、60-min limit、connection-local `store=false` 和 error codes。 | OpenAI Responses API 的公开 create/streaming reference 不等于 OpenResponses WS conformance；Stravia 应独立 capability advertise。 |
| Items/messages | `message`、roles、`output_text`、reasoning、`function_call`/`function_call_output`、status lifecycle 可映射。 | OpenResponses 把 items/semantic events 作为跨 provider core，并要求 provider extension type 加 slug 前缀。 | OpenAI 当前有大量厂商 item/tool unions；未经标准 schema 纳入的 item 只能保留为 extension/raw，不可提升为 core。 |
| Function tools | `type:"function"`、name/description/JSON Schema parameters、`strict`、`tool_choice`、parallel calls 和 call arguments delta 可映射。 | OpenResponses core FunctionToolParam 只保证 function 面；OpenAI `tools` 当前还可选多种 hosted tools。 | MCP、hosted tool execution semantics、side effects、approval、tool timeout/retry 未形成统一标准。 |
| Hosted tools | OpenResponses 正文承认 internally-hosted tools；OpenAI 官方文档列出 web search、file search 等 built-in tools。 | OpenAI 工具 schema、result item、include values 和 capability policy 属于 OpenAI API；`openai:*` 在 OpenResponses 是 provider extension 示例，不是 core。 | Stravia 不应将 OpenAI hosted tools 伪装为所有 provider 都支持；按 capability matrix 拒绝/降级并记录 loss。 |
| 多模态 input | 普通 user message 的 `input_text`/`input_image`/`input_file` 主要结构和 URL/data URL 语义相似；OpenResponses 还允许 `function_call_output.output` 携带 `input_video`。 | OpenAI 当前 `input_image.detail` 还公开 `original`；OpenResponses 2026-04-24 `ImageDetail` 仅 `low`/`high`/`auto`。OpenAI 当前文档还描述 audio input，标准 core 没有 audio。 | 普通 user-message video、audio、统一 file upload、输出图片/音频未成为 OpenResponses core。 |
| Output format | `text` 可做 plain text 或 JSON schema，`output_text` 与 refusal 可映射。 | OpenResponses schema 面更小；OpenAI structured output、model-specific JSON Schema restrictions 仍受 OpenAI product contract。 | 不同 provider 的 JSON Schema subset、strict 行为和 refusal semantics 需 adapter contract，不能假定完全一致。 |
| Reasoning | `reasoning`、summary、encrypted content、`reasoning.*` events 都有共同概念。 | OpenResponses 把 encrypted content 视为 opaque；OpenAI 的 include/caching/retention 语义更具体，且模型约束是厂商规则。 | raw reasoning 的可见性、安全、重放与加密格式没有跨 provider 标准。 |
| State/compaction | `previous_response_id` 可继续对话；OpenResponses 另有 `/responses/compact`。 | OpenResponses compact 返回 window，不是 response ID，并定义 WS continuation；OpenAI create reference 的 `context_management` 是更广的厂商字段。 | conversation persistence、zero-retention、TTL、跨连接恢复等不是统一持久化标准；Stravia 必须声明 store semantics。 |
| Errors | `type`/`code`/`message`/`param`、400/404/429/500 分层和 stream `error` event 可映射。 | OpenAI 当前错误 code 枚举更丰富，含 image/file policy codes；OpenResponses code enum 更小且允许 extension。 | Retry-After、供应商 rate-limit headers、policy taxonomy、retry safety 与 idempotency 未由标准完整规定。 |
| 端点面 | `POST /v1/responses` 可作兼容 ingress。 | OpenResponses published OpenAPI paths 是 create + `/responses/compact`；OpenAI 的厂商资源/工具和其它 lifecycle operation（例如官方 reference 暴露的 `POST /responses/{response_id}/cancel`）不自动属于标准。 | OpenAI 的 `cancel`、Files/Uploads、Conversations 等需单独 OpenAI adapter；不可对外宣称 OpenResponses 支持。 |

OpenAI 端点和字段事实以上述官方 create、streaming-events、resources reference 为准；OpenAI API 的变化不会自动改变 OpenResponses 的版本化 schema。[S9][S11][S12]

## 10. Stravia 协议网关采用建议

### 10.1 最低实现面（建议标记 `openresponses-2026-04-24-core`）

1. **Ingress/headers：** `POST /v1/responses`；Bearer `Authorization`；严格 `Content-Type: application/json`；返回 `application/json` 或 `text/event-stream`。在官方冲突解决前不接受 form body，或仅在显式非标准 compatibility flag 下接受并记录。
2. **同步路径：** `model` + string/array `input`；message roles；`input_text`；`ResponseResource` 必备 id/object/timestamps/status/model/output/usage；结构化 HTTP errors。
3. **Streaming：** `stream=true`；生成 `response.created`、`response.in_progress`、`response.output_item.added`、`response.content_part.added`、`response.output_text.delta`、`response.output_text.done`、`response.content_part.done`、`response.output_item.done`、终结 `response.completed`/`response.failed`/`response.incomplete`，并以 `[DONE]` 结束；保持 `sequence_number` 与索引。
4. **Function loop：** `FunctionToolParam`、`tool_choice`、`function_call`、`response.function_call_arguments.delta/done`、`function_call_output`、`previous_response_id` continuation。未知 tool type fail closed。
5. **Multimodal：** 至少 `input_image`（URL/data URL、`low`/`high`/`auto`）与 `input_file`（URL/data/file data）；不支持项返回 `invalid_request`，绝不静默丢弃。
6. **Observability/compatibility：** 在 response metadata 或 gateway logs 保存 pinned spec version、target/provider、原始 upstream code、字段 loss/conversion warnings；把 provider extension 放入 namespaced envelope，不污染 core object。

### 10.2 第二阶段可选面

- `POST /v1/responses/compact` + `CompactionSummaryItemParam`；
- WebSocket `response.create`、sequential-only execution、60-minute limit、`previous_response_not_found` 与 `websocket_connection_limit_reached`；
- reasoning summary/encrypted content、logprobs、JSON-schema output、`include`；
- provider-hosted tools（每个 tool 单独 capability/version），而不是宣称“OpenResponses hosted tools”全量支持。

### 10.3 兼容和安全风险

- **媒体类型冲突：** 正文 RFC2119 MUST JSON vs Reference/OpenAPI form allowance；实现以正文 MUST 为准，向 TSC 提交澄清并在版本升级时重新验证。
- **rolling OpenAI fields：** OpenAI `conversation`、`context_management`、`prompt`、`moderation`、cache fields、`user` 以及 hosted tools 会不断扩展；Stravia 需 unknown-field policy（core strict、namespaced extension）和版本化 adapter，不能把 OpenAI SDK 类型直接作为标准 schema。
- **语义丢失：** `input_image.detail=original`、audio、普通 user-message video、OpenAI tool result/include、phase、reasoning encryption 等存在 capability 差异；adapter 必须在 call 前做 representability check，拒绝或显式告警，不 silently drop。
- **状态与数据保留：** `store`、`previous_response_id`、WS connection-local state、compaction 和 gateway persistence 组合可能泄露上下文或使 continuation 失效；默认 tenant isolation、TTL、删除/eviction，并把 `previous_response_not_found` 原样可诊断返回。
- **SSE correctness：** sequence/index 错误、漏发 done、错误后没有 `response.failed`、错误地把 `[DONE]` 当 JSON，会导致 SDK hanging 或错配；为每种 lifecycle 做 conformance test。
- **工具副作用：** function call 只是模型请求，不代表 gateway 已执行；Stravia 必须分离 model output 与 tool executor，校验 `call_id`、权限、重放和幂等。

### 10.4 当前 Stravia 实现对照（静态代码核对）

当前 `stravia-core` 已有 OpenAI Responses adapter 和 `POST /v1/responses` ingress，支持同步/stream 转换、text/image message、function call、reasoning 以及主要 semantic event 骨架；这可复用，但尚不能标记为 `openresponses-2026-04-24` conformant：

- `protocol/codec/openai/responses/responses.rs` 只注册 `POST /v1/responses`；当前 proxy 未发现 `/v1/responses/compact` 或 Responses WebSocket ingress。
- `decoder.rs` 只解码普通消息中的 `input_text`/`input_image`；`input_file` 和普通 user-message video 未映射。`function_call_output.output` 非字符串值会被 JSON stringify，不能保留标准 content array 的媒体语义；未知 item/content 分支还会静默跳过。
- `stream.rs` 的常规标准事件没有连续 `sequence_number`；stream error 只发 `error` 后终止，未补 `response.failed` 与 `[DONE]`。这不满足 2026-04-24 的 event schema/lifecycle。
- Stravia 自有 item 使用 `stravia_agent_result` / `stravia_media_result`，而 Open Responses extension 规则要求 implementor slug 前缀形式（例如 `stravia:agent_result`）；若正式采用标准，需要 clean cutover 或把现有名字限定为 OpenAI-compatible vendor contract。

因此正确的演进方向是新增独立、日期固定的 Open Responses protocol ID/adapter，共享可证明无损的 IR/codec primitives；不要直接把现有 `OpenAIResponsesV1` 改名后宣称合规。

## 11. 仍未标准化、无法确认或需 TSC 澄清

1. **HTTP body media type 矛盾（已确认存在）：** 正文写 MUST `application/json`，Reference 与 OpenAPI path列 `application/x-www-form-urlencoded`。未见 changelog/charter 对此作解释；按正文执行并等待 TSC。
2. **正式版本策略：** Charter 将 release/versioning policy 交给 TSC，但公开材料以日期 artifact 为主；未找到 semver compatibility promise 或完整 deprecation window。Stravia 应固定日期版本和 schema hash。
3. **认证与 authorization semantics：** 标准要求 `Authorization` header，但没有统一 token issuance、scope、tenant identity、mTLS 或 gateway-to-provider auth；这些必须由 Stravia deployment contract 定义。
4. **Hosted tools/MCP：** 标准描述 externally-/internally-hosted tools，但没有统一 hosted tool catalog、MCP transport/schema、approval、timeout、retry、side-effect/idempotency 语义。
5. **多媒体扩展：** core 的普通 user-message input 有 text/image/file；video 仅进入返回态 `Message.content` 与 `function_call_output.output`，没有进入 `UserMessageItemParam.content`；audio、output image/audio、binary artifact lifecycle、upload API、image edit/generation 尚无可确认的跨 provider core contract。
6. **持久化与生命周期端点：** `store` 和 `previous_response_id` 的基本语义有定义；跨连接 persistence、retrieval/delete/cancel、retention/TTL、zero-retention 及 compaction interaction 不构成完整跨实现 persistence standard。
7. **错误与重试：** 标准给出主要 categories/status 和 stream error envelope，但没有完整 Retry-After、idempotency key、retryability matrix 或统一 policy/content moderation taxonomy。
8. **conformance scope：** 官网链接 acceptance tests/compliance，但本文未将其当作等同于规范版本的完整认证；Stravia 应在实现前针对 pinned OpenAPI 与正文规则跑独立 contract checks（不把 OpenAI 官方测试当 OpenResponses conformance）。

## 12. 一手来源索引（均于 2026-08-14 复核）

| ID | 官方来源 | 用途 |
| --- | --- | --- |
| S1 | [Open Responses 官网](https://www.openresponses.org/) | 项目身份、multi-provider/open-source 定位。 |
| S2 | [Technical Charter / Governance](https://www.openresponses.org/governance) | vendor-neutral mission、scope、TSC/治理、许可。 |
| S3 | [GitHub repository](https://github.com/openresponses/openresponses)；[最新 commit 92c12d9](https://github.com/openresponses/openresponses/commit/92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c) | 官方仓库、最新可识别 commit 与版本化 schema tree。 |
| S4 | [官方 Changelog](https://www.openresponses.org/changelog) | `2026-01-15` 与 `2026-04-24` dated release、WebSocket/compact/phase/logprobs 变化。 |
| S5 | [2026-04-24 OpenAPI JSON](https://www.openresponses.org/openapi/2026-04-24/openapi.json)；[仓库 schema](https://github.com/openresponses/openresponses/tree/92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c/schema) | 端点、request/response schema、item/content/event 枚举。 |
| S6 | [2026-04-24 Specification](https://www.openresponses.org/specification/2026-04-24) | RFC2119 HTTP/SSE/WS、item lifecycle、semantic events、tool-hosting、errors、continuation。 |
| S7 | [2026-04-24 Reference](https://www.openresponses.org/reference/2026-04-24) | 可读 API 参数/响应表；注意其 form-body 叙述与 S6 冲突。 |
| S8 | [最新 OpenAPI `paths`（raw）](https://raw.githubusercontent.com/openresponses/openresponses/92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c/public/openapi/2026-04-24/openapi.json) | `POST /responses`、`/responses/compact`、request media types 与 SSE union。 |
| S9 | [OpenAI 官方 Create a model response](https://developers.openai.com/api/reference/resources/responses/methods/create) | OpenAI Responses API 的厂商字段、输入、tools、端点用途；不当作 OpenResponses 标准。 |
| S10 | [OpenResponses additive patches 中的 `InputVideoContent`](https://github.com/openresponses/openresponses/blob/92c12d96d7b61d6d15e2214daa5e9c6000ab6e1c/schema/openapi_additive_patches.yaml) | video component 的定义及其加入 `Message`/`FunctionCallOutput` unions 的位置；需与未包含它的 `UserMessageItemParam` 区分。 |
| S11 | [OpenAI 官方 Responses resource reference](https://developers.openai.com/api/reference/resources/responses) | OpenAI cancel/compact resource surface 与 Response object。 |
| S12 | [OpenAI 官方 Responses streaming events](https://developers.openai.com/api/reference/resources/responses/streaming-events) | OpenAI SSE event object/current vendor event surface，对照标准事件。 |

## 结论

截至 2026-08-14，Stravia 可以采用 **Open Responses 2026-04-24** 作为独立、日期固定的 Responses-shaped ingress，但只能把它当作规范版本，不得写成“OpenAI Responses API 标准”。先实现 JSON HTTP + SSE + function loop + text/image/file core，按 capability matrix 对 OpenAI hosted tools、audio/video、WS、compaction 和厂商字段做显式扩展；针对正文与 Reference 的媒体类型冲突采取正文 MUST 的 fail-closed 行为，并跟踪 TSC 澄清/下一 dated release。
