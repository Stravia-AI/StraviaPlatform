# OpenAI Responses API WebSocket mode 研究记录

| 项 | 结论 |
| --- | --- |
| 研究截点 | 2026-08-17 |
| 范围 | 仅 OpenAI 官方文档、官方 API reference、`openai-node`/`openai-python` 官方仓库与 release；不把 Realtime API 当作 Responses WebSocket mode。 |
| 总结 | **Responses API WebSocket mode 已有公开官方文档和官方 SDK 实现，可作为公开 API 接入；但官方材料没有给出明确的 GA/稳定性声明，SDK 仍保留 `OpenAI-Beta: responses_websockets=2026-02-06` 的可选 beta header 示例。因此生产接入应按“公开可用、GA 状态未确认/需保守演进”处理。** |
| Stravia 判断 | Stravia 将下游 `/v1/responses` HTTP/SSE/WebSocket ingress 与 Provider Transport 分离。OpenAI direct 与 Codex OAuth 的可表示 generation 请求由 Model Turn Executor 优先通过 Responses WebSocket 调用上游；Embeddings 与不具备该能力的 Target 保持 HTTP。Open Responses 公共协议仍固定为 `2026-04-24`，rolling OpenAI wire 仅存在于 Vendor transport adapter。 |

## 1. 公开状态、beta/GA 标识

### 1.1 “公开可用”的证据

OpenAI 的公开开发者文档单独提供 [WebSocket mode guide](https://developers.openai.com/api/docs/guides/websocket-mode)，明确写出 Responses API 支持持久 WebSocket，并给出 `wss://api.openai.com/v1/responses`、认证 header、`response.create` 请求和完整 continuation/错误/限制说明。它不是仅在博客或 SDK 私有 API 中出现的实验功能。

官方 Node SDK 的 [Responses API 文档（v7.4.0）](https://github.com/openai/openai-node/blob/v7.4.0/docs/responses.md#responses-over-websocket)公开导出 `ResponsesWS`；官方 Python SDK 3.1.0 的 [Responses resource](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py#L1946-L1977)公开 `connect()`，文档字符串直接称其为 persistent Responses API WebSocket。Python [v3.1.0 release](https://github.com/openai/openai-python/releases/tag/v3.1.0)（2026-08-14）还记录了 WebSocket stream IDs 和独立 WebSocket events 的 API 更新。

**结论：** 就“是否有公开 endpoint、公开协议和可安装 SDK”而言，答案是 **是**。

### 1.2 GA / beta 的可核验边界

- WebSocket mode guide 本身没有 `beta`、`preview` 或 `GA` 标签；同页也没有发布日期或 SLA 承诺。[官方 guide](https://developers.openai.com/api/docs/guides/websocket-mode)
- 官方 Node SDK 文档将 Responses WebSocket 与 Realtime 分开，并说在需要时可透传 feature-specific beta header，示例为 `OpenAI-Beta: responses_websockets=2026-02-06`；官方 Node [example](https://github.com/openai/openai-node/blob/v7.4.0/examples/responses/websocket.ts#L132-L171)和 Python [example](https://github.com/openai/openai-python/blob/v3.1.0/examples/responses/websocket.py#L112-L130)都把该 header 做成**可选**的 `--use-beta-header` 开关，默认不发送。
- Python SDK 的 generated beta 类型仍保留 `BetaResponsesClientEvent`/`BetaResponsesServerEvent`（[官方源文件](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/types/beta/beta_responses_client_event.py)），但同一版本的标准 `client.responses.connect()` 已公开（[官方源文件](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py#L1946-L1977)）。这是“公开接口 + beta 兼容层仍存在”的混合信号。

**不能确认：** OpenAI 没有在上述官方文档中声明 Responses WebSocket mode 何时 GA、beta header 是否对某些组织/账户仍必需、或该功能对应的稳定性/SLA。故不应把它写成已 GA；也不应反过来声称它只能在受邀 beta 中使用。

## 2. Endpoint、认证与握手

### 2.1 Responses WebSocket mode

| 项 | 官方值 |
| --- | --- |
| WebSocket endpoint | `wss://api.openai.com/v1/responses` |
| 认证 | 握手 header `Authorization: Bearer $OPENAI_API_KEY`；API key 应只在服务端/可信 worker 使用。 |
| model 位置 | 每一轮的 `response.create` JSON body 内的 `model`；不是 URL query。 |
| 首个应用层消息 | WebSocket 建立后，客户端发送 JSON event，`type` 必须是 `response.create`。 |
| session.update | **没有** Responses WebSocket 的 `session.update` 握手；这是 Realtime 的 session/event 模型。 |

上述 endpoint、header 和首个请求由 [WebSocket mode guide 的 Connect and create responses](https://developers.openai.com/api/docs/guides/websocket-mode#connect-and-create-responses)给出。最小请求（按官方 guide；不加入 HTTP-only `stream`/`background`）为：

```json
{
  "type": "response.create",
  "model": "gpt-5.6",
  "store": false,
  "input": [
    {
      "type": "message",
      "role": "user",
      "content": [{"type": "input_text", "text": "Find fizz_buzz()"}]
    }
  ],
  "tools": []
}
```

Guide 明确说 payload mirrors normal Responses create body，但 transport-specific `stream` 和 `background` 不使用。这里与 SDK 示例存在一个应记录的兼容差异：Node/Python 示例为复用 generated 类型仍发送 `stream: true`，而 guide 要求 WebSocket 应用不要发送它；Stravia adapter 应以当前 API guide 为准，默认省略 `stream` 和 `background`，并保留对服务端返回错误的可诊断性，而不是假定二者都被接受。[Node example](https://github.com/openai/openai-node/blob/v7.4.0/examples/responses/websocket.ts#L336-L367)、[Python example](https://github.com/openai/openai-python/blob/v3.1.0/examples/responses/websocket.py#L221-L257)、[guide](https://developers.openai.com/api/docs/guides/websocket-mode#connect-and-create-responses)

### 2.2 事件/请求流程

1. 建立 WebSocket，完成 TLS/HTTP Upgrade，并在 Upgrade 中携带 Bearer auth。
2. 客户端发送一个 `response.create`。每个 subsequent turn 只发送新 `input` items，并设置上一个 response 的 `previous_response_id`。[Continue with incremental inputs](https://developers.openai.com/api/docs/guides/websocket-mode#continue-with-incremental-inputs)
3. 服务端发出与 Responses streaming event model 相同的 semantic events；常见文本路径为 `response.created` → `response.in_progress` → `response.output_item.added` → `response.content_part.added` → 多个 `response.output_text.delta` → `response.output_text.done` → `response.content_part.done` → `response.output_item.done` → `response.completed`。工具、reasoning、audio 或 hosted tool 会加入相应 typed events，不能依赖固定完整序列。[Streaming guide](https://developers.openai.com/api/docs/guides/streaming-responses#read-the-responses)、[Responses API reference](https://developers.openai.com/api/reference/resources/responses)
4. 若出现 function call，客户端执行工具，再发送新的 `response.create`，其中 `input` 只放 `function_call_output`（以及本轮新增 user item），并带 `previous_response_id`；官方 WebSocket guide 明确给出该 continuation 形状。[tool continuation example](https://developers.openai.com/api/docs/guides/websocket-mode#continue-with-incremental-inputs)
5. 终止时处理 `response.completed`；失败和不完整响应处理 `response.failed`/`response.incomplete`；连接/请求级 JSON event 为 `type: "error"`。官方 Python example 还兼容当前观察到的 `response.done` terminal event，说明客户端应在生产中记录未知/变体 terminal event，而不要只按字符串静默丢弃。[Python official example](https://github.com/openai/openai-python/blob/v3.1.0/examples/responses/websocket.py#L300-L341)

典型错误（由官方 guide 给出）包括：

- `previous_response_not_found`：要求 continuation 的 response ID 不在可用状态中；
- `websocket_connection_limit_reached`：连接达到 60 分钟上限，应重连；
- 其它请求/模型/工具错误按 Responses API error envelope 返回。[官方错误示例](https://developers.openai.com/api/docs/guides/websocket-mode#errors-to-handle)

## 3. 模型、功能与上下文限制

### 3.1 模型可用性

官方没有发布一个独立的“WebSocket model allowlist”。WebSocket `response.create` 的 body mirror normal Responses create body，所以模型资格应按该模型的 `v1/responses` endpoint support、组织权限和 model-specific limits 判断，而不是按 Realtime model 名称猜测。[WebSocket guide](https://developers.openai.com/api/docs/guides/websocket-mode#connect-and-create-responses)

官方 [Models overview](https://developers.openai.com/api/docs/models)称最新 OpenAI models 可通过 Responses API 使用；具体模型页面才是 endpoint/feature 的权威矩阵。例如：[GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna)标明 `Responses | v1/responses | Supported`，支持 streaming、structured_outputs、function_calling、file_search、web_search、code_interpreter 等；它的 model page 给出 1,050,000 context window、922,000 maximum input tokens、128,000 max output tokens。该页面同时标明 `Realtime | v1/realtime | Not supported`，因此它适合 Responses WebSocket mode 而不是 Realtime WebSocket。

反例：[GPT-Realtime-2.1](https://developers.openai.com/api/docs/models/gpt-realtime-2.1)明确 `Responses | v1/responses | Not supported`、`Realtime | v1/realtime | Supported`；不能把 Realtime model 直接填进 Responses WebSocket 的 `model`。

### 3.2 连接、并发、状态和上下文

这些是 WebSocket mode guide 明确的限制：

- 一个 WebSocket 可以收到多个 `response.create`，但逐个顺序执行，**同一连接一次只有一个 in-flight response**；当前无 multiplexing，需要并行 run 时使用多个连接。
- 单连接最长 **60 分钟**；达到上限需重连。
- active socket 只有一个 connection-local in-memory previous-response state（最近一个 response）；`store=true` 时服务端可能从持久化状态 hydrate 较旧 ID，但会失去 in-memory latency benefit。`store=false`（包括 ZDR）没有持久化 fallback，uncached ID 失败为 `previous_response_not_found`。
- continuation 失败（4xx/5xx）会从 connection-local cache evict 被引用的 `previous_response_id`，不能假设重发同一 continuation 一定可恢复。
- 断线/60 分钟后：若 `store=true` 且 ID 仍可取，重连后用 `previous_response_id`；若 `store=false`/ZDR 或 ID 不存在，必须从 `previous_response_id: null`（或省略）开始并发送完整输入上下文。[Connection behavior and limits](https://developers.openai.com/api/docs/guides/websocket-mode#connection-behavior-and-limits)、[How continuation works](https://developers.openai.com/api/docs/guides/websocket-mode#how-continuation-works)、[Reconnect and recover](https://developers.openai.com/api/docs/guides/websocket-mode#reconnect-and-recover)
- 使用 `context_management` 的 server-side compaction 时仍沿用最新 `previous_response_id`；独立 `POST /v1/responses/compact` 返回 compacted input window **而不是 response ID**，之后在 WS 上开启新 chain。[Compaction](https://developers.openai.com/api/docs/guides/websocket-mode#compaction-and-creating-new-responses)

**未公开的限制：** 官方 guide 没有给出“每组织最多多少条 WS 连接”、独立 WS concurrency quota、WS-specific RPM/TPM 或总 in-flight quota；这些不能从 60 分钟/单连接串行限制推导。仍需按具体 model 的 rate-limit 页面和账户实际响应处理 429/5xx。

## 4. SDK 支持

### Node.js / TypeScript（官方 `openai-node` v7.4.0）

- `import { ResponsesWS } from 'openai/resources/responses/ws'`；构造 `new ResponsesWS(client)`，使用 `.send(event)`、`.on('event', ...)` 或 typed event listeners。[官方 docs](https://github.com/openai/openai-node/blob/v7.4.0/docs/responses.md#responses-over-websocket)
- Node helper 依赖可选的 `ws` peer dependency；官方 source 的 `ResponsesWS` 构造函数在缺少 `ws` 时显式抛错。[`src/resources/responses/ws.ts`](https://github.com/openai/openai-node/blob/v7.4.0/src/resources/responses/ws.ts)
- helper 从 `OpenAI` client 继承 endpoint 配置；静态 `apiKey` 会自动加入 auth。官方 docs 明确说 async `apiKey` function 和 workload identity 不会被 helper 自动 resolve，须在 WebSocket options 中传入已解析的 `Authorization` header。[官方 docs](https://github.com/openai/openai-node/blob/v7.4.0/docs/responses.md#responses-over-websocket)
- source 的 URL builder 将 client base URL 的 scheme 转成 `ws`/`wss`，path 固定 `/responses`；[internal-base.ts](https://github.com/openai/openai-node/blob/v7.4.0/src/resources/responses/internal-base.ts#L60-L67)。默认公共 client base URL 因此得到 `/v1/responses`。

### Python（官方 `openai-python` v3.1.0）

- `client.responses.connect(...)` 和 `client.AsyncOpenAI().responses.connect(...)` 返回 `ResponsesConnectionManager`/async manager；连接对象提供 `send()`、`recv()`、迭代器、typed event parse 与 close。[官方 source](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py#L1946-L1977)
- source 将 client base URL scheme 转成 `ws`/`wss`，path 追加 `/responses`，并把 client auth headers 与额外 headers合并到 WebSocket Upgrade。[官方 source](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py#L4493-L4555)
- 缺少 `websockets` 依赖时 source 抛出安装 `openai[realtime]` 的错误文本；这是当前 SDK packaging 的命名陷阱：该 extra 也承载 Responses WebSocket，但并不意味着这条连接是 Realtime API。[官方 source](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py#L4514-L4519)

本记录核验了 Node 与 Python 两个官方 SDK；没有把第三方 WebSocket client 或社区 adapter 当作 SDK support 证据。其它官方 SDK 是否已在同一版本提供 Responses WebSocket helper，本文不作无来源推断。

## 5. 与 Realtime API WebSocket 的区别（不可混用）

| 维度 | Responses API WebSocket mode | Realtime API WebSocket |
| --- | --- | --- |
| Endpoint | `wss://api.openai.com/v1/responses` | `wss://api.openai.com/v1/realtime?model=...`（voice-agent session） |
| 首个协议动作 | 连接后发送 `response.create`；请求体沿用 Responses create | 用 Realtime client events 管理 session，典型是 `session.update` |
| 语义 | 长链 agent/tool workflow 的低延迟 Responses continuation；`previous_response_id` 是链路关键 | 持续 session，低延迟 audio/text、session state、response events；WebSocket 是 server-to-server raw audio transport |
| 音频 | 不是通用语音 session；可用能力仍由所选 Responses model/feature 支持决定 | Realtime session 原生承载 base64 audio chunks、text/audio responses |
| model | 选 `v1/responses` 支持的模型（例如 GPT-5.6 Luna）；Realtime-only model 不行 | 选 `v1/realtime` 支持的 model（例如 GPT-Realtime-2.1）；该 model page 标明 Responses 不支持 |
| 认证/浏览器建议 | 官方 Responses guide 给 server-side Bearer key；不要把长效 key 暴露浏览器 | Realtime guide server-to-server 用标准 API key；browser/mobile 推荐 WebRTC，并可使用 ephemeral token |
| 官方区分证据 | [Node Responses docs](https://github.com/openai/openai-node/blob/v7.4.0/docs/responses.md#responses-over-websocket)明确“different API” | [Realtime WebSocket guide](https://developers.openai.com/api/docs/guides/realtime-websocket#sending-and-receiving-events)明确 session/client events/server events 和 audio 责任 |

Realtime 官方 overview 还明确把 voice-agent session 放在 `/v1/realtime`，并在 beta→GA migration 中讨论 Realtime 自己的 GA interface；这段 GA 迁移说明**不能**被解释为 Responses WebSocket mode 已 GA。[Realtime overview](https://developers.openai.com/api/docs/guides/realtime#beta-to-ga-migration)

## 6. 生产风险与建议

1. **状态可靠性：** 默认按 60 分钟强制重连设计；`store=false`/ZDR 下必须能安全重放完整上下文，不能只保存 response ID。
2. **并行性：** 单连接不能 multiplex；连接池大小和账户级连接 quota 未公开，应做 backpressure、连接上限、指数退避和可观测的 `previous_response_not_found`/`websocket_connection_limit_reached` 处理。
3. **API 演进：** beta header、SDK generated event 命名和 guide 的 `stream` 规则存在混合信号；对 event `type` 做 forward-compatible dispatch，保留 raw event，避免只硬编码一个 terminal variant。不要无条件发送 `OpenAI-Beta`，但应提供可配置 header 以便 OpenAI 对指定账户要求时启用。
4. **凭证：** Responses WebSocket 是长连接，Bearer key 会在 Upgrade header 中出现；只允许服务端/可信 worker。浏览器直连应另行评估凭证暴露风险，不要把 Realtime 的 ephemeral-token/ WebRTC 方案照搬到 Responses。
5. **内容安全：** Responses streaming guide 警告 partial completion 更难在输出前评估，moderation scores 只在完整 output 可用后到达；生产应缓冲或采用分段安全策略，而不能把每个 `delta` 当作已审核文本。[官方 moderation risk](https://developers.openai.com/api/docs/guides/streaming-responses#moderation-risk)
6. **功能边界：** model page 的 tool/feature matrix 才是能力来源；不要因为 WebSocket 能承载 `response.create` 就推断所有 hosted tools、audio 或某模型均支持。对 function call、reasoning encrypted content、compaction 和 model-specific fields 做 capability gating。

## 7. Stravia 当前边界与可接入性

### 7.1 已有 ingress

- `backend/crates/stravia-core/src/proxy/server.rs:45-55`：`GET /v1/responses` 路由到 `open_responses::websocket::handler`，`POST /v1/responses` 路由到 `open_responses::responses::handler`；`POST /v1/responses/compact` 另有 route。
- `backend/crates/stravia-core/src/proxy/ingress/open_responses/responses.rs:1-75`：HTTP JSON parse、Bearer 认证、Open Responses 2026-04-24 decode 和 dispatch；`background=true` 明确返回 unsupported。
- `backend/crates/stravia-core/src/proxy/ingress/open_responses/websocket.rs:51-76`：WebSocket Upgrade 前做 Origin 检查与 Bearer auth，单消息上限 100 MiB。
- `.../websocket.rs:95-216`：一条连接维护 single in-flight flag；非 `response.create` 返回 400，并发返回 409 `response_in_progress`；连接 TTL 60 分钟。
- `.../websocket.rs:218-330`：WS handler 调用统一 Inference Run，并把 Delivery 产生的 SSE JSON frame 转成 WS text、丢弃 SSE `[DONE]`。下游 ingress transport 不决定上游 transport；同一 run 可由 OpenAI/Codex Responses WebSocket Provider Transport 执行。

### 7.2 上游实现边界

- `backend/crates/stravia-core/src/proxy/dispatcher/inference_run/provider.rs` 在 Vendor request 已生成、canonical parser 之前选择 HTTP/SSE 或 Responses WebSocket。两种 transport 产出同一 canonical stream delta 与 typed error，不复制 Inference Run、Hook、Response Chain 或 Delivery 生命周期。
- `backend/crates/stravia-core/src/proxy/client.rs` 使用 `reqwest-websocket` 和 Provider 已配置的 `reqwest::Client`，因此复用 HTTP/HTTPS proxy、CONNECT、TLS 与 proxy authentication。OpenAI direct 使用 `/v1/responses`；Codex channel 使用 ChatGPT backend `/responses` 并添加其 beta/request/session/thread metadata。
- 每个上游 socket 只允许一个 in-flight response，最长 60 分钟。当前没有本地连接数量上限和 idle 回收；无 affinity 的终态连接关闭，保留 affinity 的连接在过期或失败时驱逐。
- 明确 unsupported endpoint 按 Target namespace 缓存；瞬时 connect/TLS/proxy failure 进入短 cooldown。只有 frame 写入前的安全失败回退同 Target HTTP/SSE；401/403/429、写入后不确定断线、malformed/binary event 和 Client Output Commit 后失败不重放。
- Response Chain 在完整 Effective Model Request 上选择严格 canonical item 前缀。`store=false` 只复用同 socket cache tip；`store=true` 可按 Provider 能力跨连接 hydrate。内部 upstream response ID 不进入其它下游协议。
- `AllowedWebSocketOrigins` 仍只约束下游浏览器 ingress。standalone 与 desktop 通过统一装配函数注入 allowlist；无 `Origin` 的 server-side/CLI client 不受此检查影响。

**当前建议：** 把 upstream Responses WebSocket 视为 Model Turn 的透明 transport 优化，而不是公共协议扩张。能力或 affinity 不可用时恢复完整 Effective Model Request；不得削弱 representability、store、Hook 或 Client Output Commit 语义。

### 7.3 历史 ingress provider 验证（2026-08-17）

使用官方 `openai-python 3.1.0` 的 `client.responses.connect()`，连接隔离 Stravia server 的 `ws://127.0.0.1:19637/v1/responses`。Stravia 配置 Xiaomi custom provider（`https://api.xiaomimimo.com/v1`、`openai-compatible`）和 `mimo-v2.5`；credential 仅通过测试进程环境注入，不写入本文或仓库。

同一 WebSocket 上完成两轮：

1. `store=false` root turn：收到 `response.created`、reasoning/text semantic events 和 `response.completed`，状态 `completed`，输出严格为 `FIRST_OK`。
2. `store=false` continuation：使用第一轮 Gateway response ID 作为 `previous_response_id`，收到新的完整 semantic event 序列和 `response.completed`，状态 `completed`，输出严格为 `SECOND_OK`。

客户端进程两轮总 wall time 为 7.39 秒；该值包含本地 gateway/SDK 开销，不能当作 Xiaomi 单独模型延迟。这次 **2026-08-17 的历史验证** 证明 OpenAI SDK Responses WS client → Stravia WS ingress → OpenAI-compatible HTTP/SSE → Xiaomi MiMo → WS semantic events 的单连接 continuation 路径可工作；它发生在 upstream WebSocket Provider Transport 实施前，不作为当前 OpenAI/Codex egress 或故障矩阵的验证证据。

## 8. 无法确认项（官方一手来源未给出）

1. Responses WebSocket mode 的 GA 日期、SLA、版本兼容承诺和废弃策略。
2. `OpenAI-Beta: responses_websockets=2026-02-06` 是否在任一当前组织/区域/模型上必需；公开 guide 示例不发送，SDK 示例把它做成可选。
3. 独立 WS 每组织连接数上限、跨连接并发 quota、WS-specific RPM/TPM；公开 guide 只给单连接 60 分钟、单连接串行和“无 multiplexing”。
4. WebSocket 专属的完整 model allowlist；应以所选 model page 的 `v1/responses` endpoint support 和实际账户授权为准。
5. `response.completed` 与 Python 示例兼容的 `response.done` 在所有账户/模型上的精确终止事件契约；客户端必须保留 raw event 和错误遥测。

## 官方来源索引

- [O1 WebSocket mode guide](https://developers.openai.com/api/docs/guides/websocket-mode)
- [O2 Streaming API responses](https://developers.openai.com/api/docs/guides/streaming-responses)
- [O3 Responses API reference](https://developers.openai.com/api/reference/resources/responses)
- [O4 Models overview](https://developers.openai.com/api/docs/models)
- [O5 GPT-5.6 Luna model page](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
- [O6 GPT-Realtime-2.1 model page](https://developers.openai.com/api/docs/models/gpt-realtime-2.1)
- [O7 Realtime overview](https://developers.openai.com/api/docs/guides/realtime)
- [O8 Realtime WebSocket guide](https://developers.openai.com/api/docs/guides/realtime-websocket)
- [O9 openai-node v7.4.0 Responses docs](https://github.com/openai/openai-node/blob/v7.4.0/docs/responses.md#responses-over-websocket)
- [O10 openai-node v7.4.0 WebSocket class](https://github.com/openai/openai-node/blob/v7.4.0/src/resources/responses/ws.ts)
- [O11 openai-node v7.4.0 URL builder](https://github.com/openai/openai-node/blob/v7.4.0/src/resources/responses/internal-base.ts#L60-L67)
- [O12 openai-node v7.4.0 example](https://github.com/openai/openai-node/blob/v7.4.0/examples/responses/websocket.ts)
- [O13 openai-python v3.1.0 Responses resource](https://github.com/openai/openai-python/blob/v3.1.0/src/openai/resources/responses/responses.py)
- [O14 openai-python v3.1.0 example](https://github.com/openai/openai-python/blob/v3.1.0/examples/responses/websocket.py)
- [O15 openai-python v3.1.0 release](https://github.com/openai/openai-python/releases/tag/v3.1.0)
