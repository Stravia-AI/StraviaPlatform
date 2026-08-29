# Open Responses 2026-04-24 协议融合设计

> 状态：已实施
> 决策记录：[`ADR-0011`](../adr/0011-own-open-responses-as-a-dated-protocol.md)  
> 规范调研：[`Open Responses 开放标准研究`](../research/open-responses-standard.md)  
> 规范快照：2026-04-24  
> 范围：`stravia-core` canonical IR、protocol codecs、proxy ingress、Provider adapters、Response Chain、Server/Desktop transport、devtools 与协议文档  
> 非目标：完整复制 OpenAI rolling Responses product semantics、`background` execution、response compaction、动态 extension plugins、旧协议身份兼容迁移

---

## 1. 结论

Stravia 将以 **Open Responses Protocol** 作为唯一 Responses-shaped canonical baseline，并固定到官方 `2026-04-24` dated release。它是独立于 OpenAI rolling Responses API 的协议身份，不是现有 `OpenAIResponsesV1` 的别名。Ingress 兼容 rolling 客户端的 additive fields 与 hosted tools，但这些扩展不会隐式成为 canonical semantics。

协议转换继续服从 ADR-0006 的 pair-bound seam，但 canonical model 收敛为有序、双向的 `AiItem` Graph。所有 request、response、stream、continuation 与跨协议转换先恢复 canonical 语义，再由目标协议编码。能等价表达的语义必须转换；不能表达的内容、身份、引用、结构和硬约束必须在 provider call 前拒绝；additive metadata、响应装饰以及没有被 required/named 选择的 hosted tools 可以为兼容性静默省略。

对外能力声明固定为：

> Open Responses 2026-04-24 Stravia profile：支持 JSON HTTP、SSE 和 WebSocket；Ingress 接受结构安全的 rolling additive surface；`background` 与 `compact` 暂不支持；跨协议路径属于 compatibility-first、hard-semantics-gated conversion。

---

## 2. 决策边界

### 2.1 目标

- 为 Responses-shaped traffic 提供一个日期固定、可测试、vendor-neutral 的协议身份。
- 同一 canonical IR 支撑同步、SSE、WebSocket、continuation、工具调用和多模态。
- OpenAI、Anthropic、Google 与未来 Provider adapters 只拥有 provider-specific transport 和 wire mapping，不拥有 canonical 语义。
- 以明确的 hard-semantics gate 约束兼容性省略。
- 让 Stravia 自有能力通过可验证的 Namespaced Extension 扩展，而不是伪装成标准字段。
- 以 vendored 官方 OpenAPI、永久 contract tests 和一次性官方 acceptance run 证明 pinned profile。

### 2.2 非目标

- 不承诺实现 OpenAI rolling Responses API 的全部服务端语义；无法 canonicalize 的 additive surface 只做透传或兼容性省略。
- 不提供第二个名为 OpenAI Responses 的协议身份。
- 不实现 `POST /v1/responses/compact` 的实际 compaction。
- 不实现 `background=true`、retrieve、cancel 或轮询状态机。
- 不提供动态 extension registry 或第三方运行时代码加载；未知 `owner:*` Namespaced Extension 不做隐式透传。
- 不保证跨协议路径具有 Open Responses strict conformance。
- 不为未发布的旧 `openai-responses` 身份保留 alias、shim 或数据迁移。

---

## 3. 术语与协议身份

### 3.1 Canonical identity

| 层级 | 唯一身份 |
|---|---|
| Rust protocol variant | `Protocol::OpenResponses` |
| 配置/CLI short name | `open-responses` |
| dated constant | `OPEN_RESPONSES_2026_04_24` |
| wire protocol ID | `open-responses/responses/2026-04-24` |
| HTTP resource | `/v1/responses` |

`openai-compatible` 继续只表示 OpenAI Chat Completions-compatible surface。`openai-responses`、`OpenAIResponsesV1`、旧 devtools 名称和相关 aliases 全部删除。

### 3.2 规范优先级

官方 2026-04-24 artifacts 不一致时，解释顺序固定为：

1. RFC2119 normative Specification；
2. dated OpenAPI artifact；
3. dated Reference 页面；
4. 官方 acceptance tests 的 observable behavior。

已知冲突必须写入 permanent contract matrix，不通过宽松解析同时接受互斥形态。当前明确采用 JSON request body；Reference/OpenAPI 暴露的 `application/x-www-form-urlencoded` 不覆盖 Specification 对 `application/json` 的要求。

### 3.3 Namespaced Extension

核心协议之外的能力必须使用 `<owner-canonical-slug>:<name>`。Stravia 自有扩展固定使用 `stravia:*`；Provider 扩展使用其 canonical slug。未知无前缀字段不能形成隐式 core extension，未知或未注册的 namespaced extension 也不能直接透传。

Extension registry 为 adapter-private、静态、启动后冻结的 validated registry。本设计不引入动态插件系统。

---

## 4. 模块 seam 与所有权

目标模块结构：

```text
protocol/
├── codec/
│   ├── open_responses/       # Open Responses 2026-04-24 wire contract
│   ├── openai/               # OpenAI Chat Completions wire contract
│   ├── anthropic/
│   └── google/
├── ir/                       # canonical AiItem Graph
└── transform/                # pair-bound decode/encode sessions

proxy/ingress/
├── open_responses/           # HTTP/SSE/WebSocket ingress
├── openai_compatible/
├── anthropic/
└── google/

provider/
├── openai/                   # auth, URL, Codex quirks, Target capabilities
├── anthropic/
└── google/
```

所有权规则：

| 模块 | 拥有 | 不拥有 |
|---|---|---|
| Open Responses codec | dated request/response/item/event wire types、schema validation、event lifecycle | OpenAI auth/URL、routing、quota |
| canonical IR | message/content/tool/reasoning/refusal/reference/usage/error 的语义 | HTTP headers、SSE framing、wire indices、provider extensions |
| pair-bound transform | decode/encode、representability、stream session | provider transport、Target selection |
| vendor adapter | auth、base URL、HTTP quirks、Resolved Target capabilities | 第二套 Responses canonical model |
| dispatcher | Inference Run、Hook、Target selection、retry、tool continuation、delivery | provider-specific JSON |
| Response Chain | durable canonical history、branch、principal scope | live Inference Run mutable state |

`stream_only` 是 Resolved Target execution capability，由具体 vendor adapter 声明。Codex 等 adapter 可声明 `true`；普通 Open Responses adapter 默认 `false`。不新增 DB/admin 开关，也不通过运行时 probe 推断。

---

## 5. Canonical `AiItem` Graph

### 5.1 唯一事实源

Request、response、continuation 和 cross-protocol conversion 共用一个有序、双向的 `AiItem` Graph。`AiItem` 至少表达：

- 稳定 item ID；
- item status；
- provenance 与 audience；
- typed body；
- 在 message 上的 role、phase 和有序 content；
- 在 tool call/output 上的 `call_id` 与生命周期；
- reasoning、refusal、annotations、usage 和 error 的结构化语义。

输入和输出方向由 typed constructors、decoder validation 和 representability gate 约束，不再用两套 graph 类型重复 message/tool facts。

### 5.2 Clean cutover

以下重复事实删除：

- `AiRequest.system`；
- `AiResponse.content`；
- `AiResponse.reasoning_content`；
- `AiResponse.tool_calls`；
- 与 `AiItem` 并行的旧 `ResponseItem` graph；
- Hook 私有 `ContextItem` model。

Hook、dispatcher 和 codecs 通过 graph iterator/query views 读取连续 item，不复制或重新拼装历史。可无分配表达的 view 不返回新的 `Vec` 或 `String`。

### 5.3 Typed bodies

Core typed bodies 覆盖：

- message：System、Developer、User、Assistant；
- content：text、image、file，以及已存在 canonical media；
- output content：output text、refusal、annotations；
- reasoning：summary/content/encrypted content；
- function call 与 function call output；
- item reference；
- extension wrapper。

`function_call_output.output` 保留官方允许的字符串或 content-array 语义，不再对 array 做 JSON stringify。

### 5.4 Instructions 与 Developer role

顶层 `instructions` 是独立 request field，不转换为 System/Developer message。Continuation 可按第 8 节继承它。

Open Responses `developer` message 在目标协议有等价角色时原样映射。目标协议无 Developer role 时，按已接受的兼容例外静默降级为 System。这是 representability fail-closed 原则的唯一初始 role 例外，必须在 ADR 和 contract tests 中固定。

### 5.5 Stream model

`AiStreamDelta` 替换为 lifecycle-complete `AiStreamEvent`。Canonical event 表达：

- response created/in-progress/terminal；
- output item added/done；
- content part added/done；
- text/refusal/reasoning/function arguments delta/done；
- usage 和 typed error。

Wire-only state留在目标 stream session：`output_index`、`content_index`、`sequence_number`、SSE event name、WebSocket framing 和 `[DONE]`。

---

## 6. HTTP 与 response object

### 6.1 Request validation

`POST /v1/responses` 只接受：

```http
Authorization: Bearer <token>
Content-Type: application/json[; parameters]
```

- Bearer scheme 大小写不敏感。
- 重复 Authorization、空 token、其它 auth scheme 或替代 API-key headers 均拒绝。
- 接受 JSON media type 参数，例如 `application/json; charset=utf-8`。
- form body 返回 415 `unsupported_media_type`。
- HTTP body 上限沿用 100 MiB；超过后返回 413。
- JSON parse、schema 和 semantic validation 在 dispatcher/provider call 前完成。

根请求要求 `model` 和 `input`。带 `previous_response_id` 的 continuation 可以省略 `input` 并继承 parent model；显式值始终覆盖继承值。

### 6.2 Fixed Stravia effective profile

Open Responses 规范没有为所有省略字段定义统一 server default。Stravia 固定以下 profile，并在 ResponseResource 中回显 effective values：

| 字段 | Stravia default |
|---|---|
| `temperature` | `1` |
| `top_p` | `1` |
| `presence_penalty` | `0` |
| `frequency_penalty` | `0` |
| `top_logprobs` | `0` |
| `parallel_tool_calls` | `true` |
| `truncation` | `disabled` |
| `service_tier` | `default` |
| `tool_choice` | `auto` |
| `text` | `{ "format": { "type": "text" } }` |
| `tools` | `[]` |
| `metadata` | `{}` |
| `store` | `true` |
| `background` | `false` |

Conformant same-protocol upstream 明确返回的 provider-executed effective values 优先；upstream 未回显时使用 Stravia effective profile。Gateway-owned response ID、logical model、store、metadata、safety identifier 和 local lifecycle 不被 upstream 覆盖。

### 6.3 ResponseResource

Gateway 生成 response ID、timestamps 和 lifecycle status。`model` 始终返回客户端选择的 logical Model ID，不泄漏 Provider Model ID。ResponseResource 必须包含 dated schema 的 required keys；不再输出非标准 top-level `output_text` convenience field。

Usage 仅在 input/output totals、cached tokens 和 reasoning tokens 全部可信时输出完整 object。任一组成部分未知时输出 `usage: null`，不得用 `0` 表示 unknown。

---

## 7. Streaming

### 7.1 SSE

每个 SSE frame 必须满足：

- `event:` 等于 JSON body 的 `type`；
- body 符合对应 dated event schema；
- 不发送 SSE `id:`；
- 每个最终发出的事件具有严格递增、无重复的 `sequence_number`；
- 生命周期按 response → item → content part → delta/done → terminal 排序；
- terminal response event 后发送 `[DONE]`。

Provider event 的原始 sequence、indices 和 response ID 不直接透传。目标 stream session 基于最终输出重新分配 sequence 和 indices，保证 Hook、tool loop 或协议转换插入/删除事件后仍满足 wire ordering。

### 7.2 WebSocket

WebSocket 复用 `/v1/responses` resource。客户端每轮发送标准 `response.create` event；一条连接内顺序执行多个 response，但同一时刻最多一个 in-flight response。

固定策略：

| 约束 | 行为 |
|---|---|
| 并发 create | 返回 WS error，`status=409`、`code=response_in_progress`；当前 run 和连接继续 |
| Auth | handshake 解析 Bearer；每轮重新检查 key enabled/expiry、Model binding 与 quota |
| Origin | 有 Origin 时复用 proxy CORS allowlist；无 Origin 的 CLI/SDK 允许 |
| 单消息上限 | 100 MiB |
| outgoing queue | 64；发送方等待背压，不丢事件 |
| 连接寿命 | 60 分钟硬上限 |
| 单 run deadline | 300 秒，服从更严格的现有 policy |
| heartbeat | 不新增协议外 ping/pong message |
| disconnect | 立即取消 in-flight Inference Run |

WebSocket 不在 `response.create` body 中重复传 token。所谓“每轮重验”是使用 handshake credential 重新读取当前认证与授权状态，而不是缓存 60 分钟授权结果。

`store=false` 时，最近 response 只存在于连接本地；新连接不可恢复。`store=true` 时，continuation 通过 durable Response Chain 恢复，不依赖同一 socket。

### 7.3 上游 Responses WebSocket

下游 WebSocket ingress 与上游 Provider transport 是两个独立 seam。OpenAI direct 与 Codex OAuth 的 generation Target 使用上游 Responses WebSocket；Chat Completions、Open Responses、Anthropic Messages、Gemini 以及 stream/non-stream 客户端均经过同一个 Inference Run，Embeddings 仍走 HTTP。URL 从 Vendor adapter 生成的 Responses HTTP URL 映射为 `ws:`/`wss:`，握手复用同一 `reqwest::Client` 的 HTTP/HTTPS proxy、CONNECT、proxy authentication、TLS 与 Provider headers。Codex adapter 固定 `store=false`、当前 `OpenAI-Beta` 与 request/client metadata；rolling wire 差异不进入 dated Open Responses 公共协议。

连接池按精确 Target namespace 和 upstream response ID 建立 affinity；每个连接同一时刻只执行一个 response。连接 60 分钟后不再复用；当前不设置本地连接数上限，也不回收仍有 affinity 的 idle 连接，因此高并发分支会增加文件描述符、内存与上游连接占用。`store=false` 只允许同 socket 最近 tip 续接；排队 sibling、重启、断线或 max-age 失去 affinity 后在新 socket 发送完整 Effective Model Request。取消关闭所属 socket。

握手 400/404/405/426/501 会把当前 Target namespace 标记为不支持，短暂连接错误进入 15 秒 cooldown；两者都只在请求尚未被上游接受时回退同 Target HTTP/SSE。401/403/429 保持 typed Provider rejection。发送后断线、malformed/binary event 与未知接受状态不重放。`previous_response_not_found` 仅在尚无 client-visible event 时失效旧 affinity，并在同 socket 全量重放一次；再次失败或已有可见输出立即终止。

---

## 8. Response Chain、store 与 continuation

### 8.1 Gateway identity

客户端只看到 Stravia response ID。`previous_response_id` 永远引用 Stravia Response Chain 节点；upstream response ID 是 Target continuation state，不能成为公共身份。

Response Chain 是协议无关 Generation Chain 的 Responses 投影。Stravia 对每个完整交付的生成请求保存 canonical graph，并沿用 Principal isolation、分支语义和现有可配置 TurnChain TTL；默认 7 天。图片等二进制内容以 ArtifactRef 和 metadata 保存，不把 Base64 写入 TurnChain JSON。

### 8.2 `store`

| client `store` | Stravia | same-protocol upstream |
|---|---|---|
| 省略/`true` | durable 保存 canonical Response Chain | 发送 `store=true`，允许 Provider 持久化和复用 upstream response ID |
| `false` over HTTP | durable 保存 Generation Chain；后续 `previous_response_id` 可由 Stravia materialize | 发送 `store=false` |
| `false` over WebSocket | durable 保存 Generation Chain；connection-local state 仍可降低同 socket continuation 开销 | 发送 `store=false` |

`store` 是 Upstream Store Hint，不是 Stravia 数据保留开关。`metadata` 和 `safety_identifier` 始终由 Stravia 本地拥有，不发送远端 Provider。它们只在 dated schema 允许的位置存储和回显。

只有完整交付给客户端的 `completed` 和 `incomplete` terminal response 才提交 Response Chain。`failed`、客户端断线、delivery failure 或取消不提交，避免 durable history 指向客户端从未完整观察到的节点。

### 8.3 Continuation merge

Continuation materialize 的逻辑顺序固定为：

```text
parent input → parent output → new input
```

显式 request-level config 覆盖 parent effective config；省略的 `instructions`、tools、tool choice、reasoning、text format、sampling 和其它可继承配置使用 parent effective values。根请求仍必须显式提供 model 和 input。

当 same-protocol Target、Provider instance、Provider Model 和有效配置均可安全复用，且 upstream 已持久化 parent 时，adapter 可以使用 upstream continuation ID；否则必须从 canonical graph 展开历史。该优化不得改变 Gateway response identity 或 representability checks。

### 8.4 Reusable Response Prefix

客户端没有显式父节点且提交完整历史时，Stravia 可自动发现已完成 Response Chain 的最长严格等价前缀。发现发生在历史恢复、Request Hook、Vendor canonical mutation、默认 effective profile 归一化与 representability gate 之后；Hook 始终观察完整逻辑历史。比较只在完整 canonical `AiItem` 边界进行，不切分 content block 或字符，不做文本/原始 wire 模糊匹配。reasoning summary/content/signature、tool correlation、Unknown block、角色、ArtifactRef 与媒体 metadata 均参与语义比较。

候选必须属于同一 Principal、精确 Target、Provider 账号与 credential/config generation、base URL、resolved model 和 Open Responses egress，并具有相同 instructions、tools/tool choice、reasoning controls、response format、sampling 与其它请求约束。只有已完整交付、upstream terminal 为 `completed`、upstream response ID 存在且 UpstreamResponse/ClientOutput Hook 未改变输出的节点写入索引；`incomplete`、`failed`、取消、delivery failure 与本次请求完整相等的节点不是自动候选。最长 `item_count` 优先；同长度按 `completed_at`、node ID 降序确定性选择。显式 `previous_response_id` 始终优先且允许空 input。

### 8.5 Item reference

Ingress 接受 `{ "id": "..." }`；若显式提供 `type`，必须等于 `item_reference`。解析顺序：

1. 当前 request graph；
2. 当前 WebSocket connection-local graph；
3. 当前 Principal 可见的 stored Response Chain。

未命中统一返回 HTTP 400 `item_reference_not_found`。跨 Principal、已过期和从未存在使用同一错误，避免泄漏其它主体的 item existence。

---

## 9. Tools 与 Stravia extensions

### 9.1 Function loop

标准 function tool 保留 `name`、description、JSON Schema parameters、strict、tool choice、parallel choice、call ID、arguments delta/done 和 output content。

纯客户端 function call 终结当前 response；客户端下一次请求创建新的 Inference Run。只有同轮同时出现 Platform Tool 和客户端 function call 时，才保留现有 mixed-only、process-local `ToolContinuation`：隐藏 Platform Tool 继续执行，客户端 call 等待 output 后恢复同一个 run。

### 9.2 Platform Tool wire identity

平台工具采用 namespaced request tool：

- `stravia:web_search`
- 后续显式注册的其它 `stravia:*`

不保留 OpenAI hosted-tool aliases，不为 Stravia 私有工具制造无前缀 core 类型。

### 9.3 Final output policy

Stravia extension 只暴露最终结果，不暴露私有 progress events：

| 能力 | 最终 wire output |
|---|---|
| Web Search | 标准 assistant message；文本内保留 inline citations；只有 offsets 可靠时生成 annotations |
| Agent | `stravia:agent_result` final item |
| Media Understanding | `stravia:media_result` final item |

Extension final item 仍使用标准 `response.output_item.added` / `response.output_item.done` 生命周期。

---

## 10. Representability policy

### 10.1 必须等价转换

目标协议有等价表达时必须转换，不能因为字段名不同而丢弃：

| Canonical 语义 | Open Responses | Anthropic | Google |
|---|---|---|---|
| text | text content | text block | text part |
| image | image content | image block | inline/file data |
| function call | `function_call` | `tool_use` | `functionCall` |
| function output | `function_call_output` | `tool_result` | `functionResponse` |
| previous response | native ID 或 canonical expansion | canonical history → messages | canonical history → contents |
| item reference | resolve item | resolve 后编码 block | resolve 后编码 part |
| JSON Schema output | `text.format` | 仅在等价 contract 可证明时 | `responseSchema` |

### 10.2 必须拒绝

以下语义在目标 Target 不可表达时，必须在 provider call 前拒绝：

- text/image/file/media content；
- function output 和 call correlation；
- response/item identity 与 reference；
- required output format 或 strict schema；
- `tool_choice=required/none`、allowed tools；
- output/tool call limits；
- truncation 和其它安全、成本、上下文完整性硬约束。

拒绝必须指出具体 parameter/item 和目标 capability 缺口，不能把内容塞进 prompt 假装等价。

### 10.3 Compatibility omission

跨协议转换允许静默省略不改变任务事实、工具硬要求、输出硬结构、成本上限或身份关联的 advisory surface：

- `temperature`
- `top_p`
- `presence_penalty`
- `frequency_penalty`
- `reasoning.effort`
- `service_tier`
- `prompt_cache_key`
- `client_metadata` 与其它 additive metadata
- `include`、`stream_options.include_obfuscation`、`top_logprobs`、`text.verbosity`
- `tool_choice=auto/none` 下目标协议不能执行的 hosted tools

兼容性判断按语义类别而不是 dated field allowlist。`required`/named/allowed tools、`max_tool_calls`、required output format、truncation 等硬约束仍适用 10.2。

### 10.4 Conformance boundary

Open Responses `2026-04-24` 定义 canonical baseline；Ingress 可以接受其 additive superset。Open Responses Target 原样接收 compatibility envelope，跨协议 Target 只承诺 canonical semantics 与硬约束，不宣称完整 rolling Responses conformance。

---

## 11. Provider interaction 与失败边界

同协议合法标准字段原样发送 upstream，不建立静态 per-field Provider capability matrix；具体 Target 是否支持由 upstream 决定。合法 request 得到 upstream 4xx 时，Stravia 规范化 error envelope 并返回，不切换 Target。

Target failover 仅沿用现有可重试 transport/5xx policy。绝不因 schema、auth、quota、unsupported parameter 或其它 4xx 改投不同 Provider，以免重复副作用或改变语义。

同协议 Target 出现以下情况时视为 provider protocol violation：

- 未注册 namespaced extension；
- schema-invalid item/event；
- 非法 item/content/response lifecycle；
- 无法重建单调 sequence 或 call correlation。

处理为 fail closed：

- HTTP/SSE commit 前：规范化 HTTP error；
- stream commit 后：发送 `error` → `response.failed` → `[DONE]`；
- 不猜测修复硬语义、不因该错误切换 Target；结构安全的 additive rolling 字段不属于 protocol violation。

原始 Provider code/message/body 只进入受现有 redaction policy 管理的内部日志；客户端只看到稳定 Stravia error taxonomy。

---

## 12. Unsupported surfaces

### 12.1 Background

`background=true` 返回 HTTP 400：

```json
{
  "error": {
    "type": "invalid_request",
    "code": "unsupported_feature",
    "param": "background",
    "message": "Background responses are not supported by this Stravia profile."
  }
}
```

不创建假 response、不异步执行、不提供 retrieve/cancel/polling endpoint。

### 12.2 Compact

保留 `POST /v1/responses/compact` route，使客户端得到协议错误而不是 router 404。Request 先经过 auth、content type、JSON 和 schema validation；合法 compact request 返回 HTTP 400 `unsupported_feature`，`param=compact`。

不调用 Provider compaction、不做本地摘要、不生成伪 compact resource。WebSocket compact continuation 同样不支持。

---

## 13. Error lifecycle

所有客户端可见文案默认英文。错误 envelope、HTTP status 与 stream terminal state保持一致。

| 场景 | 结果 |
|---|---|
| auth 缺失/无效 | 401 canonical error |
| Origin 不允许 | 403 canonical error/拒绝 upgrade |
| 非 JSON media type | 415 `unsupported_media_type` |
| body/message 超限 | 413 `request_too_large` |
| JSON/schema/semantic invalid | 400 `invalid_request` |
| previous response 不可见/过期 | 400 `previous_response_not_found` |
| item reference 不可见/过期 | 400 `item_reference_not_found` |
| representability failure | 400 `unsupported_feature` 或具体 invalid parameter code |
| WS 已有 in-flight response | WS error 409 `response_in_progress`，连接保持 |
| pre-commit Provider failure | normalized HTTP error |
| post-commit stream failure | `error` → `response.failed` → `[DONE]` |

`[DONE]` 是 transport terminator，不是 canonical item，也不计入 `sequence_number`。

---

## 14. Schema、验证与合规

### 14.1 Vendored artifact

仓库固定保存官方 immutable `2026-04-24` OpenAPI JSON，并记录：

- 官方 dated URL；
- upstream commit；
- SHA-256；
- license/notice。

Production 不加载或解释该 artifact；production wire types 为手写 owned Rust types。OpenAPI 仅作为 contract-test oracle。

### 14.2 Permanent tests

增加仅测试使用的 Rust `jsonschema` dev-dependency，按 OpenAPI 3.1 / JSON Schema 2020-12 验证 fixture。永久测试至少覆盖：

- root request 与 continuation required fields；
- ResponseResource required keys 和 fixed effective profile；
- message/content/tool/output unions；
- function output content array；
- video union 所在的 dated schema 边界；
- 所有 SSE schemas 均要求 `sequence_number`；
- event name/body type 一致；
- response/item/content lifecycle；
- HTTP content type、auth 和 error envelopes；
- WebSocket sequential turns、busy、reauthorization、disconnect；
- `store=true/false`、branch、TTL、Principal isolation；
- item reference resolution；
- representability reject 与 silent-drop allowlist；
- extension registry 与 final-only output；
- Provider protocol violation fail-closed；
- compact/background unsupported contracts。

RFC2119 MUST/SHOULD matrix 和已知官方 artifact 冲突作为测试资料长期保留。

### 14.3 Official acceptance run

实施完成后临时启动本地 server glue 并运行官方 2026-04-24 acceptance runner，只选择 Stravia profile 宣称支持的 tests。结果用于本次交付证据；通过后删除临时 runner/server/credential glue。

Vendored OpenAPI、hash、Rust schema tests、behavior tests 和 contract matrix永久保留。Compact/background tests 必须验证明确 unsupported contract，不能以跳过伪装实现。

---

## 15. Clean-cutover migration

实施是一次性 clean cutover，不保留双路径：

| 当前 | 目标 |
|---|---|
| `OpenAIResponsesV1` | `OpenResponses` |
| `OPENAI_RESPONSES_V1` | `OPEN_RESPONSES_2026_04_24` |
| `openai-responses/...` | `open-responses/responses/2026-04-24` |
| `protocol/codec/openai/responses` | `protocol/codec/open_responses` |
| `proxy/ingress/openai_responses` | `proxy/ingress/open_responses` |
| OpenAI hosted tool aliases | registered `stravia:*` tools |
| response convenience fields + parallel graph | ordered `AiItem` Graph |
| `AiStreamDelta` | lifecycle-complete `AiStreamEvent` |

必须同步迁移 registry、routing、Provider protocol selection、devtools、Server/Desktop route、frontend protocol labels、tests 和文档。旧协议身份未发布，因此不迁移旧 DB/config/log string，不保留 aliases 或 deprecated re-exports。

`docs/design/architecture.md` 和用户文档在代码 cutover 完成前继续描述当前实现；实施完成并通过 smoke test 后，再把它们切换到新身份，避免 architecture/README 提前声称尚不存在的行为。

---

## 16. 与既有决策的关系

- ADR-0006 的 pair-bound conversion seam、canonical ownership 和 representability gate继续有效；本设计取代其中 `AiRequest/AiResponse/AiStreamDelta` 作为最终 canonical shape 的部分。
- ADR-0004 的 Web Access ownership、隐藏 Platform Tool round 和 Provider selection继续有效；本设计取代 OpenAI-native Responses tool identity 和非标准 wire output。
- ADR-0001 的 Inference Run 与 Turn Chain 生命周期边界继续有效；Response Chain 仍是 Turn Chain 的协议投影。

---

## 17. 验收场景

1. **HTTP root**：合法 JSON root request 返回 dated ResponseResource、Gateway ID、logical Model ID 和完整 required keys。
2. **SSE tool call**：function arguments delta/done、output item done、terminal response 与 `[DONE]` 顺序正确，每个 JSON event 的 sequence严格递增。
3. **WebSocket sequential turns**：同一连接完成两轮；第二轮使用 `previous_response_id`；并发 create 得到 409 且第一轮不中断。
4. **Store false**：HTTP response 完成但不进入 durable chain；后续 HTTP continuation统一 not found；同 socket 可继续，断线后不可继续。
5. **Durable branch**：两个请求从同一 stored parent 分支，互不共享 Inference Run mutable state。
6. **Reference isolation**：本 Principal item 可解析；其它 Principal、过期和未知 item 返回相同错误。
7. **Cross-protocol tool result**：function output 等价映射到 Anthropic/Google；目标不支持时 provider call 前拒绝。
8. **Hard constraint**：目标无法表达 strict JSON Schema 或 required tool choice 时拒绝，不静默降级。
9. **Advisory hint**：跨协议仅允许第 10.3 节列出的字段静默省略。
10. **Extension final-only**：Web Search 返回标准 assistant message；Image/Agent/Media 只返回 registered final item，无私有 progress event。
11. **Protocol violation**：same-protocol upstream 返回未注册 event，Stravia fail closed；已 commit stream 发标准失败序列。
12. **Unsupported surface**：background 与 compact 返回明确 400 contract，无假实现、无 404。
13. **Schema proof**：上述 fixtures 通过 vendored 2026-04-24 schema；artifact hash 与来源一致。

---

## 18. 已接受风险

- Developer → System 降级完全静默，目标协议无法观察原 role 差异。
- 第 10.3 节 sampling/operational hints 可被静默省略，跨协议输出可能因此变化。
- 合法同协议字段不做静态 Target preflight，Provider 可能在执行前以 4xx 才报告不支持。
- `store=true` 同时允许远端 Provider 保存 prompt/output；只有 metadata 与 safety identifier 强制留在 Stravia。
- HTTP 与 WebSocket 都允许 100 MiB buffered JSON，需要依赖现有连接、并发和内存边界。
- Compact/background 未实现，因此 Stravia profile 不是官方完整 acceptance surface。
- 日期固定会拒绝后续 rolling Provider 字段；升级必须通过新的 dated protocol decision，而不是静默放宽当前 identity。
