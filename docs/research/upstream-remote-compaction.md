# 上游原生上下文压缩（Remote Compaction）能力研究记录

| 项 | 结论 |
| --- | --- |
| 研究截点 | 2026-11-02 |
| 范围 | 仅厂商官方文档、官方 API reference、官方 SDK/源码仓库。覆盖 Anthropic Messages API、OpenAI Responses API、`openai/codex` 客户端源码、Google Gemini API 与 Vertex AI、xAI Grok、Amazon Bedrock、Microsoft Foundry、OpenRouter、DeepSeek、Mistral、阿里云百炼（Qwen/DashScope）、Moonshot Kimi、智谱 GLM。不把“应用侧自己写 summary”当作原生压缩。 |
| 总结 | 具备**可转发的服务端原生压缩契约**的只有三家：Anthropic（`compaction` / `context_management` 两套 beta）、OpenAI（`context_management`、`compaction_trigger`、`POST /responses/compact` 三种形态）、xAI（`POST /v1/responses/compact`，OpenAI 兼容形状）。Google Gemini 只在 **Managed Agents（Interactions API 的 `agent`）** 内有自动压缩（约 135k tokens），`generateContent`/模型级 Interactions 没有压缩参数，Live API 的 `contextWindowCompression` 只作用于实时会话，因此没有可被外部 compact 请求驱动的契约。Bedrock 的 Claude 走 InvokeModel/`/anthropic` 可用，Converse 明确不支持；Vertex AI 的 Claude（Google managed）可用，Vertex 上的 Gemini 不可用。其余上游（OpenRouter、DeepSeek、Mistral、Qwen、Kimi、GLM）官方文档未发现任何原生压缩端点或参数。 |
| Stravia 判断 | 远端压缩必须**原样转发**：上游返回的不透明/签名结果不可本地重建，也不可跨 Target 重放。对没有原生能力的上游，Stravia 只能返回 `compaction_unsupported`，不得本地合成摘要；对已绑定到某 Target 的压缩状态，跨 Target 使用时返回 `compaction_target_mismatch`（现有代码语义，见第 8 节）。 |

## 1. 判定口径

本文把“压缩”拆成四种互不等价的能力，避免把缓存或截断误判为 compaction：

- **(a) 独立压缩操作（standalone）**：一次请求把整段历史交给服务端，返回一个可继续使用的压缩结果（通常是不透明/签名块或压缩后的窗口），本次不产生正常回复。
- **(b) 生成过程中的服务端自动压缩（inline）**：普通生成请求里带阈值/触发器参数，服务端在请求内部压缩较早上下文并继续生成。
- **(c) 上下文缓存（caching）**：复用相同前缀的 KV/计算结果以降成本与延迟，**不改变上下文长度，不产生摘要**。
- **(d) 实时会话窗口压缩（Live/session）**：长连接会话内滑动窗口丢弃/压缩旧轮次，只对会话有效，不能用于无状态 `generateContent` 请求。

判定标签：

| 标签 | 含义 |
| --- | --- |
| 原生支持 | 官方文档明确给出端点或请求参数，并说明返回结果如何回传 |
| 部分支持 | 仅某一子形态/某一平台/某一产品面可用，其余不可用 |
| 不支持 | 官方文档明确说明不支持，或能力面根本不存在对应操作 |
| 无法确认 | 一手来源未覆盖，或两家一手来源互相矛盾且无法用文档消解 |

“可回放到其它上游”单列：签名/加密块是否可跨厂商继续使用，取决于签名验证方与密钥归属；**没有任何厂商声明支持跨厂商回放**。

## 2. Anthropic（Claude Messages API）

Anthropic 有两套**互相独立**的 beta 机制，header、参数、响应形状都不同，不能混用。总览页明确“on-demand 覆盖更多常见场景”，threshold 用于普通请求内自动压缩。[A1]

### 2.1 On-demand compaction（`compact-2026-09-04`）

**请求**：在 `POST /v1/messages` 上加 header `anthropic-beta: compact-2026-09-04`，body 加顶层参数：

```json
{
  "model": "claude-opus-5-5",
  "max_tokens": 4096,
  "system": "...",
  "tools": [],
  "messages": [ ... ],
  "compaction": { "type": "summarize", "instructions": "可选，最多 16384 字符" }
}
```

- `compaction.type` 只能是 `"summarize"`；`instructions` 可选，**完全替换**服务端默认摘要 prompt，空白字符串视同未提供，`maxLength: 16384`。[A2][A7]
- 摘要调用使用请求自身的 model、`system`、`tools`、thinking 设置与 `max_tokens`；summarizer 会读工具定义但**不会执行工具**，响应里不含 thinking。`max_tokens` 限制整次调用（含摘要前的 thinking），官方建议给到数千 token。[A2]

**响应**：只有一个 `compaction` block，`stop_reason` 为 `"compaction"`，顶层 usage 归零：

```json
{
  "content": [
    { "type": "compaction", "content": "Summary ...", "signature": "EuYBCkQY..." }
  ],
  "stop_reason": "compaction",
  "usage": {
    "input_tokens": 0,
    "output_tokens": 0,
    "iterations": [{ "type": "compaction", "input_tokens": 144, "output_tokens": 276 }]
  }
}
```

- 计费/限流按 `usage.iterations` 里的 `compaction` 条目统计；顶层 `input_tokens`/`output_tokens` 为 0 是因为没有生成正常回复。把 block 回传不会再次产生压缩费用。[A2]
- API reference 的 block 结构还包含可选的 `encrypted_content` 与 `tool_changes`（后者在带 `inline-tools-2026-09-15` 等 beta 时记录压缩区间内的工具增删）；指南示例只展示 `content` + `signature`。`content: null` 表示压缩失败，服务端把这种 block 当 no-op，空字符串不允许。[A7][B1]
- `usage.iterations` 条目还可能出现 `"type": "message"`（threshold 机制下），见 2.2。

**流式**：block **整块**到达——一个携带完整 block 的 `content_block_start`，随后 `content_block_stop`，**没有** `content_block_delta`；`ping` 可能出现在前后或之间。[A2]

**回传规则（adoption）**：

1. block 放在 `messages` 最前面（可作为独立的 `assistant` message，或作为首条 message 的第一个 content block，首条 message 可以是 `user` 或 `assistant`）。
2. 被摘要的消息必须删除；若 block 前面还留着被摘要消息 → 400 `compaction_block_misplaced`。
3. 每个请求**恰好一个** `compaction` block，且每个携带 block 的请求都要带 beta header。
4. block 必须**逐字节**按返回原样回传（含 `signature`）；改动会得到 400 `compaction_signature_invalid` 或 `compaction_content_mismatch`。Python 里若手工序列化，`model_dump()` 会多出 `citations: null`/`text: null` 而被拒，需用 `to_dict()` 或 `model_dump(exclude_none=True)`。[A2]

**错误与“没有摘要”**：

| 情形 | 返回 |
| --- | --- |
| 瞬时服务端问题（生成或读取 block） | 529 `overloaded_error`，`error.details.error_code = compaction_unavailable` |
| block 前仍有被摘要消息 | 400 `compaction_block_misplaced` |
| `signature` 或 `content` 被改 | 400 `compaction_signature_invalid` / `compaction_content_mismatch` |
| 多个 compaction block | 400 |
| 最后一个 `assistant` 轮以未回填结果的 tool call 结尾 | 400 |
| `messages` 无任何 user/assistant 内容 | 400 `compaction_nothing_to_summarize` |
| 漏带 beta header | 400（消息提示 `requires anthropic-beta: compact-2026-09-04`；携带 block 的请求报“compaction 不是预期 block 类型”） |
| 回传时夹带额外字段（如 `citations: null`） | 400 校验错误 |

摘要**未产生**时仍是 200，但 `content` 为空，`stop_reason` 为摘要调用自身的结束原因：`max_tokens`（截断）、`model_context_window_exceeded`（放不下摘要 prompt）、`tool_use`（模型改去调工具）、`refusal`、`end_turn`（无文本）。调用仍计费并计入 `usage.iterations`；无摘要时可继续对话、稍后再压缩。[A2]

**限制（会 400 的组合）**：不能与 `context_management` 同请求；不能带 `stop_sequences`、结构化输出 `output_config.format`、`tool_choice` 为 `any`/`tool`；不能带 `output_config.task_budget.remaining`；`max_tokens: 0` 不可用（Bedrock 文档列出）；对话必须仍能放进模型 context window（要先压缩再溢出）；`POST /v1/messages/count_tokens` 接受 `compaction` 但**忽略**它。[A2][B1]

**模型/平台**：`claude-fable-5-1`、`claude-mythos-5-1`、`claude-fable-5`、`claude-mythos-5`、`claude-mythos-preview`、`claude-opus-5-5`、`claude-opus-5`、`claude-sonnet-5-5`、`claude-sonnet-5`、`claude-haiku-5-5`、`claude-opus-4-8`、`claude-opus-4-7`、`claude-opus-4-6`、`claude-sonnet-4-6`；平台为 Claude API、Claude Platform on AWS、Claude Models on Google Cloud（Google managed）、Microsoft Foundry，**均标 beta**。[A2]

### 2.2 Threshold compaction（`compact-2026-01-12`）

**请求**：header `anthropic-beta: compact-2026-01-12`，body：

```json
{
  "model": "claude-opus-5-5",
  "max_tokens": 4096,
  "messages": [ ... ],
  "context_management": {
    "edits": [
      {
        "type": "compact_20260112",
        "trigger": { "type": "input_tokens", "value": 150000 },
        "pause_after_compaction": false,
        "instructions": "可选，完全替换默认摘要 prompt"
      }
    ]
  }
}
```

- `trigger.type` 只支持 `input_tokens`；默认 150,000，最小 50,000。[A3]
- `pause_after_compaction: true` 时，生成完 compaction block 即停，`stop_reason` 为 `"compaction"`，便于调用方在继续前插入内容（例如保留最近几轮）。[A3]
- 触发点在每次 sampling iteration 开始时检查；带 server tools（如 web search）时，一个请求内可能压缩多次。[A3]

**响应**：`compaction` block 出现在 assistant content 的**最前面**，后面继续正常文本：

```json
{
  "content": [
    { "type": "compaction", "content": "Summary ..." },
    { "type": "text", "text": "Based on our conversation so far..." }
  ]
}
```

回传方式是**整条 response 照常 append**，API 自己丢弃 block 之前的内容（与 on-demand 相反）。多次压缩时，最后一个 block 代表最终状态。[A3]

**流式**：`content_block_start` → **一个**携带完整摘要的 `content_block_delta`（`delta.type` 为 `compaction_delta`）→ `content_block_stop`；摘要不逐 token 流式。[A3]

**usage**：带该 beta 时每个响应都有 `usage.iterations`；发生压缩时出现 `compaction` 条目 + 主 `message` 条目。顶层字段只等于非压缩迭代之和，**不含** compaction 用量，统计成本必须累加 `usage.iterations`。[A3]

**已知缺陷**：请求里定义了 `tools` 时，模型偶尔会在内部摘要步骤去调工具，此时返回 `compaction` block 但 `content: null`；服务端把 null block 当 no-op。官方建议在 `instructions` 里明确“不要调用任何工具，只输出文本”。[A3]

**与 on-demand 的互斥**：不能同时发 `compaction` 和 `context_management`；`compact_20260112` 也不能运行在携带签名 block 的请求上。[A2]

**平台**：Claude API、Claude Platform on AWS、**Amazon Bedrock**、**Amazon Bedrock (InvokeModel)**、Claude Models on Google Cloud（Google managed）、Microsoft Foundry；另有 Managed Agents 面“always on”，每次压缩发 `agent.thread_context_compacted` 事件。[A3]

### 2.3 “保留最近几轮”与“后台压缩”不是服务端参数

两者都是**客户端编排模式**，复用同一个 `compaction` 请求：

- **Keep recent turns**：没有“保留 N 轮”的参数；调用方自己选切点，只把较早的消息发进 compaction 请求，返回的 block 放在保留轮次之前。保留轮次必须原样发送（含 thinking block），切点不能落在 tool call 与其结果之间。[A5]
- **Background / async compaction**：压缩请求与对话并行；调用方记录发送时的消息条数，block 到达后**精确替换**那批消息，期间新增的轮次留在 block 之后。压缩请求照常占速率限制，且期间应用同时有两个请求在跑。[A4]
- 两者可组合；`instructions` 也可用于自定义摘要 prompt。[A1][A2]

### 2.4 与 preserved thinking 的交互

在启用 preserved thinking 的模型上，block 之后“保留轮次”里的 thinking 只有在以下条件全部成立时才继续有效：

1. 自该 thinking 产生以来的**每一次** compaction 都跑在 preserved-thinking 模型上；
2. 保留轮次**紧接**被摘要消息、且逐字节未改；首条保留消息角色必须与最后一条被摘要消息不同，且不能是对话中段的 `role: "system"` 消息；
3. `system` 与未标 `defer_loading: true` 的 `tools` 在压缩请求、产生 thinking 的请求、后续请求之间保持一致。

条件不满足时压缩本身不报错，失效发生在**下一次**携带该 thinking 的请求（由 `thinking.block_binding.prefix_mismatch_behavior` 决定行为，需额外的 `thinking-binding-controls-2026-08-01` beta）。想安全改 `system`/`tools`：先压缩整段对话（不保留轮次），再改。带 `inline-tools-2026-09-15` 时，block 会多一个 `tool_changes` 字段记录压缩区间内的工具增删，需原样回传。[A6]

### 2.5 平台可用性（Anthropic 一手来源）

| 平台 | On-demand `compact-2026-09-04` | Threshold `compact-2026-01-12` | 证据 |
| --- | --- | --- | --- |
| Claude API | beta | beta | [A2][A3] |
| Claude Platform on AWS | beta | beta | [A2][A3][A8] |
| Amazon Bedrock（`/anthropic/v1/messages`） | Anthropic 表未列；AWS 文档称可用 | beta | [A2][A3][A11][B1] |
| Amazon Bedrock（InvokeModel，Opus 4.6 及更早） | Anthropic 表未列；AWS 文档称可用 | beta | [A3][B1] |
| Amazon Bedrock（Converse / ConverseStream） | **不支持** | **不支持** | [B1] |
| Claude Models on Google Cloud（Google managed，`aiplatform.googleapis.com/.../publishers/anthropic/models/...:rawPredict`） | beta | beta | [A2][A3][A9][A10] |
| Microsoft Foundry | beta | beta | [A2][A3] |
| Claude Managed Agents | 不适用（另一套会话压缩） | always on | [A3] |

**需要记录的矛盾**：Anthropic 的 on-demand 页面 `supportedPlatforms` **没有** Amazon Bedrock，但 AWS 的 Bedrock 文档明确写出 `compact-2026-09-04` 通过 `InvokeModel`、`InvokeModelWithResponseStream`、`CountTokens` 可用，只是 Converse 不支持。[A2][B1] 两边都是厂商一手来源，无法用文档消解；工程上应按“Bedrock 侧需逐模型/逐路由探测，不能仅凭 Anthropic 表判断”处理。

**Bedrock 侧额外细节**（同一套 Messages 形状，beta 放在 body 的 `anthropic_beta` 数组）：

- `compact-2026-01-12` 在 Bedrock 上只列了 `anthropic.claude-sonnet-4-6` 与 `anthropic.claude-opus-4-6-v1`。[B1]
- `compact-2026-09-04` 的模型支持与旧 beta 不同，需查各模型卡；不支持时返回 400 `compaction: Extra inputs are not permitted`。[B1]
- `instructions` 在摘要前会过服务端分类器：被标记 → 200 空 `content` + `stop_reason: "refusal"`；分类器不可达 → 529 `the compaction instructions could not be checked; retry the request`（fail closed）；平台禁用自定义指令 → 400 `compaction.instructions is not available on this platform`。[B1]
- 签名相关失败：block 被改 → 400 `compaction block content does not match its signature` / `invalid signature in compaction block`；签名瞬时失败 → 529 `the compaction summary could not be signed; retry the request`。[B1]

## 3. OpenAI（Responses API）

OpenAI 有**三种**原生形态，且互相独立。[O1][O2][O4]

### 3.1 行内自动压缩：`context_management`

```json
{
  "model": "gpt-5.3-codex",
  "input": [ ... ],
  "store": false,
  "context_management": [{ "type": "compaction", "compact_threshold": 200000 }]
}
```

- `context_management` 是数组，元素 `{type, compact_threshold}`；`type` 目前只支持 `"compaction"`，`compact_threshold` 最小 1000。[O3]
- 渲染后的 token 数越过阈值时，服务端在同一次请求内压缩，**不需要**额外调用 `/responses/compact`；流里会带加密的 compaction item，并在继续推理前裁剪上下文。[O1]
- ZDR：`store=false` 时该模式对 ZDR 友好。[O1]
- 无状态数组链式调用时照常 append output；用 `previous_response_id` 时只发新 user 消息。若手工 append，可以丢掉“最近一个 compaction item 之前”的 items 以缩小请求；用 `previous_response_id` 时**不要**手工裁剪。[O1]

### 3.2 行内显式触发：`compaction_trigger` input item

- 输入 items 里加 `{"type": "compaction_trigger"}`，文档写死“**必须是最后一个 input item**”。[O3]
- 流式事件：`response.compaction.compacting` 最多每 30 秒报告一次进度，**不携带摘要内容、不修改 compaction item**；item 生命周期仍由 `response.output_item.added` / `response.output_item.done` 表示，`response.output_item.done` 携带最终 `encrypted_content`。短压缩可能完全不发进度事件。[O4]
- 输出 item 形状：`{"type": "compaction", "id"?: string, "encrypted_content": string}`，`encrypted_content` 最大 104857600 字符。[O2]

### 3.3 独立端点：`POST /responses/compact`

| 项 | 官方值 |
| --- | --- |
| 方法/路径 | `POST /responses/compact`（base 为 `https://api.openai.com/v1`） |
| 请求体 | `model`（必填）、`input`（string 或 items 数组，最多 131072 项）、`instructions`、`previous_response_id`、`prompt_cache_key`、`prompt_cache_options`、`prompt_cache_retention`、`service_tier` |
| 响应 | `CompactedResponse`：`id`、`created_at`、`object: "response.compaction"`、`output`（压缩后的 items 数组）、`usage` |
| 流式 | reference 未列出 `stream` 参数；返回单个 JSON 对象 |
| 语义 | 完全无状态、ZDR 友好；返回的窗口是“下一个规范上下文窗口”，应**原样**作为下一次 `/responses` 的 input，不要裁剪 |

`output` 里不只有 compaction item，还可能保留上一窗口的部分 items；compaction item 形如 `{"type": "compaction", "encrypted_content": "...", "id": "..."}`。压缩后的内容“opaque and not intended to be human-interpretable”。送进 `/responses/compact` 的窗口必须仍能放进模型 context window——压缩不能救一个已经超限的请求。[O1][O2]

### 3.4 `openai/codex` 客户端当前行为

对 `openai/codex` 仓库 commit `3026a8e54e0f48c4d8074cd700f35b0697abc329`（2026-11-02）做全仓检索：

- 远端压缩走**普通 Responses 流式请求**：`input.push(ResponseItem::CompactionTrigger {})` 把 trigger 作为最后一个 input item，然后 `client_session.stream(...)` 收流，从 `response.output_item.done` 收集 compaction item。[O6]
- 全仓 `codex-rs` **没有** `responses/compact` 字符串；当前客户端不调用独立 compact 端点。[O6]
- V2 窗口由客户端自己拼装（`compact_remote_v2.rs` 的 `build_v2_compacted_history`）：保留用户消息（`RETAINED_MESSAGE_TOKEN_BUDGET` = 64000 token，最新优先）加那一个 compaction item，服务端返回的其它输出 item 被忽略；流异常最多重试 `MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES` = 2 次。[O6]
- 请求通过 `x-codex-turn-metadata` 头标注 `request_kind: "compaction"` 与 `compaction{trigger, reason, implementation, phase, strategy}`；这是客户端分析元数据，Stravia 未复刻。[O6]
- 仓库里还有本地/远端两套 compaction 实现与 `features.async_compaction`、`model_async_compact_threshold_percent` 等开关，说明“后台压缩”在客户端侧编排，而不是服务端参数。[O6]

## 4. Google Gemini / Vertex AI

### 4.1 (a) 独立压缩操作：**不存在**

- `models.generateContent` 的请求体字段为 `contents`、`tools`、`toolConfig`、`safetySettings`、`continuationToken`、`labels`、`systemInstruction`、`generationConfig`、`cachedContent`、`serviceTier`、`store`，**没有** compaction/summarize 类字段，也没有对应端点。[G1]
- Interactions API（`POST /v1beta/interactions`）的 `generation_config` 只有 `seed`、`stop_sequences`、`tool_choice`、`tools`、`thinking_level`、`thinking_summaries`、`max_output_tokens` 等；整份 reference 中检索不到 `compact`。[G2]
- 没有“把历史压成一个可回放块”的返回类型。

### 4.2 (b) 行内服务端自动压缩：**仅 Managed Agents**

- Managed Agents（Interactions API 里 `agent="antigravity-preview-..."`）文档写明：长会话的 reasoning steps、tool calls、大文件会迅速占满上下文，**Managed Agents API 在约 135k tokens 处自动执行原生 context compaction**，无需参数。[G4][G5]
- 这是 agent 运行时内部行为：没有开关、没有阈值参数、没有返回给调用方的可回放压缩块；普通模型级 Interactions 调用与 `generateContent` 都没有该行为。[G2][G3][G4]

### 4.3 (c) 上下文缓存：**是缓存，不是压缩**

- `cachedContents`（显式缓存）与 implicit caching 只复用相同前缀的计算结果以降成本/延迟；缓存命中体现在 `usage.total_cached_tokens`，**不改变上下文长度、不生成摘要**。[G1][G8]
- Interactions API 只支持 implicit caching；显式缓存需回到 `generateContent`。[G8]

### 4.4 (d) Live API 会话窗口压缩：**只对 Live 会话有效**

- `BidiGenerateContentSetup.contextWindowCompression` 接受 `ContextWindowCompressionConfig`；其中 `slidingWindow` 是唯一的 `compressionMechanism`，`triggerTokens` 为触发阈值（未设置时默认模型窗口的 80%）。服务端在上下文超过配置长度时**自动缩小上下文**。[G6][G7]
- 官方 session management 文档明确它用于把音频会话从 15 分钟 / 音视频 2 分钟延长到不限时长，属于长连接会话机制。[G6]
- 它不在 `generateContent` 请求体里，也没有“返回压缩结果”的语义，因此不能用于无状态压缩请求。[G1][G7]

### 4.5 `countTokens`：只计数

`models.countTokens` 返回 token 数（`totalTokens`），没有压缩副作用或压缩选项。[G9]

### 4.6 Vertex AI

- **Gemini on Vertex AI**：`generateContent` 参考页的请求/响应字段中检索不到 `compact`、`contextWindowCompression`、`context_management`；与 Gemini API 同一套 schema，无压缩能力。[G10]
- **Claude on Vertex AI（Google managed）**：Anthropic 把 “Claude Models on Google Cloud (Google managed)” 列为两套 compaction beta 的支持平台；该产品走 `aiplatform.googleapis.com` 的 `publishers/anthropic/models/{model}:rawPredict` / `:streamRawPredict`，请求体与 Messages API 基本一致，但 `model` 在 URL、`anthropic_version` 在 body。[A2][A3][A9][A10]
- **未确认**：Google managed 页面本身没有说明 `anthropic-beta` header 如何传递（该页完全没提 beta）；跨平台的具体 header 机制属于未验证项。

### 4.7 Gemini 判定

| 形态 | 判定 | 说明 |
| --- | --- | --- |
| (a) standalone | 不支持 | 无端点、无参数、无返回块 |
| (b) inline（模型级） | 不支持 | `generateContent` / 模型级 Interactions 无压缩参数 |
| (b) inline（Managed Agents） | 原生支持 | 约 135k tokens 自动压缩，无参数、无回放块 |
| (c) caching | 支持但非压缩 | `cachedContents` / implicit caching |
| (d) Live API | 支持但仅会话 | `contextWindowCompression.slidingWindow` + `triggerTokens` |

**综合：部分支持**——只有 Managed Agents 的自动压缩是原生压缩，且无法被外部“compact 请求”驱动。

## 5. xAI Grok

- **独立端点**：`POST /v1/responses/compact`，请求 `{model, input}`；响应为 OpenAI 兼容的 `response.compaction` 对象：`id`（`cmp_<uuid>`）、`object`、`created_at`、`model`、`output`（**只含一个** compaction item：`{type:"compaction", id, encrypted_content}`）、`usage`（含 `input_tokens`、`output_tokens`、`total_tokens`、`dropped_message_count`）。[X1][X2]
- **回传**：把 `output` 数组原样作为下一次 `/v1/responses` 的 input 开头，新 user 轮追加在后面；`encrypted_content` 是 opaque，文档明确“**only meaningful when sent back to xAI's API**”，不能解析、编辑或手工合并多个 blob。[X1]
- **限制**：被压缩的对话必须已经能放进 context window；每次调用**最多一次**压缩；可以再次压缩已压缩的会话；压缩调用本身计费，可用更小模型。[X1]
- **行内压缩**：`POST /v1/responses` 的 `context_management` 字段文档写的是“Optional context-management directives (e.g. compaction). **Parsed but not yet executed**”——即接受但尚未执行，不能当作可用能力。[X2]
- **流式**：compact 端点 reference 未列 `stream`；返回单个 JSON 对象。[X2]
- **SDK**：xAI SDK 提供 `client.chat.compact_context(...)`、`chat.compact()` / `compact_context`（异步同名），属于同一服务端能力的封装。[X1]

**判定：原生支持（仅 standalone）；inline 不支持（参数被解析但不执行）。**

## 6. 其它上游逐个核对

### 6.1 Amazon Bedrock

- **Claude 模型 + InvokeModel**：两套 Anthropic beta 都可用，beta 放 body 的 `anthropic_beta`；`compact-2026-09-04` 还支持 `InvokeModelWithResponseStream` 与 `CountTokens`。[B1]
- **Claude 模型 + Converse / ConverseStream**：AWS 明确写“Neither mechanism is supported by the Converse API”。[B1]
- **Claude 模型 + `/anthropic/v1/messages`**：这是 Bedrock 上“新”的 Messages API 路由（`bedrock-mantle.{region}.api.aws/anthropic/v1/messages`），请求体与一方 API 相同，支持 `anthropic-beta` header；Anthropic 的 threshold 表把 “Amazon Bedrock” 列为支持平台。[A11][A3]
- **Bedrock 的 OpenAI 兼容路由**：`https://bedrock-runtime.{region}.amazonaws.com/openai/v1` 提供 `POST /responses`，文档中检索不到 `compact` 或 `/responses/compact`。[B2]
- **判定：部分支持**——Anthropic 模型经 InvokeModel/`/anthropic` 可用；Converse 与 OpenAI 兼容路由不支持。

### 6.2 Microsoft Foundry

- Anthropic 的 on-demand 与 threshold 页面都把 “Microsoft Foundry” 列为 beta 支持平台。[A2][A3]
- Microsoft Learn 的 “Claude models in Microsoft Foundry” 能力表只列了 **Context editing**（`context-management-2025-06-27`），没有 compaction 条目；Foundry 的 Claude 接入页也没有 compaction 字样。[F1]
- **判定：部分支持/无法完全确认**——厂商（Anthropic）声明支持；平台方（Microsoft）文档未独立记录。工程上应按“声明可用、需实测”处理。

### 6.3 OpenRouter

- 官方只有 **Context Compression** 插件（`plugins: [{id: "context-compression"}]`）：从 prompt **中间删除或截断**消息直到放进上下文窗口；消息条数超限时保留首尾各一半。它是**有损截断**，不是摘要、不返回任何可回放状态。[R1][R2]
- 官方 cookbook 对“长会话压缩”的建议是用第三方摘要模型 `scaledown/summarize` 在应用侧做摘要，再保留最近轮次——即客户端方案。[R3]
- OpenRouter 文档索引（`llms.txt`）中没有 `/responses/compact` 端点；Responses API 参考也没有 compaction 参数。
- **判定：不支持**（只有 middle-out 截断）。

### 6.4 DeepSeek

- Responses API 兼容表明确写：`context_management` **Not supported**；`truncation` **Not supported**（超窗直接 400）；`prompt_cache_key`/`prompt_cache_retention` 不支持（缓存自动管理）。文档还说明“不支持的参数会被静默忽略，不报错”。[D1]
- 没有 compact 端点，也没有 compaction item 类型。
- **判定：不支持。**

### 6.5 Mistral

- Responses API 处于 public preview，文档第一句就写“**stateless**”：历史由应用放在 `input` 里每次重发；支持项列表中没有 compaction。[M1]
- Agents / Conversations API 会把历史存到云端（`conversation_id`、`get_history`、`store=False` 可退出存储），但文档没有摘要/压缩/裁剪参数。[M2]
- 官方文档索引（`llms.txt`）检索不到 compact。
- **判定：不支持。**

### 6.6 阿里云百炼（Qwen / DashScope）

- 多轮对话指南：Qwen API 是 **stateless**，每次请求都要带历史；官方建议“Use truncation, summarization, or retrieval to manage context and reduce token consumption”——即应用侧策略。[Q1]
- OpenAI 兼容的 Responses 文档支持 `previous_response_id` 等服务端状态，但检索不到 `context_management` 或 `compact`。[Q2]
- **判定：不支持。**

### 6.7 Moonshot Kimi

- 官方多轮对话指南：Kimi API **无状态**，需手动维护 `messages`；建议“截断历史消息，控制上下文长度”，并提到“对被遗弃的消息做一次总结，生成一条新消息加入 `messages` 列表”——客户端摘要。[K1]
- Messages API（Anthropic 兼容）与 Responses API 文档中检索不到 `compact` / `context_management`；beta header 也没有 compaction 项。[K2][K3]
- **判定：不支持。**

### 6.8 智谱 GLM

- 官方文档索引（`docs.bigmodel.cn/llms.txt`、`docs.z.ai/llms.txt`）与对话补全 API reference 中检索不到 compaction / 上下文压缩参数；只有**上下文缓存**（隐式缓存，`usage.prompt_tokens_details.cached_tokens`）。[Z1][Z2]
- **判定：不支持**（缓存不是压缩）。

### 6.9 ChatGPT-Codex backend

- 公开可核验的是 `openai/codex` 客户端：当前 commit（`ba0d0e1f`，2026-11-02）的远程压缩 V2（`core/src/compact_remote_v2*.rs`）在普通流式 Responses 请求末尾追加 `compaction_trigger`，从 `response.output_item.done` 取**恰好一个** `compaction` item，其余输出 item（例如助手回复）忽略；新窗口由客户端拼装为“保留的用户消息（预算 `RETAINED_MESSAGE_TOKEN_BUDGET` = 64000 token，最新优先，边界消息截断文本）+ compaction item”。不调用 `/responses/compact`。[O6]
- Codex 后端本身是私有服务，没有公开 API reference。
- 实测（2026-11-02，ChatGPT OAuth 账号，经 Stravia Codex 插件）：`POST https://chatgpt.com/backend-api/codex/responses` 的 **HTTP SSE** 请求带末尾 `compaction_trigger` 被接受，返回 compaction item（`encrypted_content` 约 2 KB、带 `id`/`metadata`），把“保留的用户消息 + compaction item”原样作为下一次 `/responses` 的 input，模型能复述被压缩前的细节；同账号 `POST …/responses/compact` 返回 `404`。
- **判定：支持（`compaction_trigger` 流式路径已实测；`/responses/compact` 不可用）。**

## 7. 对比表

| 上游 | 独立 compact 端点 | 行内压缩控制 | 不透明/签名结果 | 可回放到其它上游？ | 流式形态 | 来源 |
| --- | --- | --- | --- | --- | --- | --- |
| Anthropic Messages（on-demand） | 否（用同一 `/v1/messages` + 顶层 `compaction`） | 否（该请求本身即压缩） | 是：`compaction` block + `signature` | 未声明；签名由 Anthropic 校验，按不可跨厂商处理（推断） | block 整块：`content_block_start` → `content_block_stop`，无 delta | [A2][A7] |
| Anthropic Messages（threshold） | 否 | 是：`context_management.edits[compact_20260112]` + `trigger` | 是：`compaction` block（可带 `cache_control`） | 同上 | `content_block_start` → 单个 `compaction_delta` → `content_block_stop` | [A3] |
| OpenAI Responses（`context_management`） | 否 | 是：`[{type:"compaction", compact_threshold}]` | 是：`compaction` item（`encrypted_content`） | 未声明；加密不透明，按不可跨厂商处理（推断） | 正常 SSE + 流内 compaction item | [O1][O3] |
| OpenAI Responses（`compaction_trigger`） | 否 | 是：最后一个 input item | 是 | 同上 | `response.compaction.compacting` 进度 + `output_item.done` | [O3][O4] |
| OpenAI `/responses/compact` | 是 | 否 | 是 | 同上 | 未列 `stream`，返回 `CompactedResponse` | [O1][O2] |
| xAI Grok | 是：`POST /v1/responses/compact` | 否（`context_management` “parsed but not yet executed”） | 是：`encrypted_content`，文档称只对 xAI 有意义 | 否（文档明确 only meaningful when sent back to xAI） | 未列 `stream`，返回 `response.compaction` | [X1][X2] |
| Google Gemini（模型级） | 否 | 否 | — | — | — | [G1][G2] |
| Google Gemini Managed Agents | 否 | 是（自动，约 135k，无参数） | 否（无回放块） | — | agent 事件流（如 `step.stop`） | [G4][G5] |
| Google Gemini Live API | 否 | 是（`contextWindowCompression`） | 否（仅会话窗口） | — | 双向 WebSocket | [G6][G7] |
| Bedrock（Claude / InvokeModel） | 否（同一 Messages 形状） | 是（两套 beta） | 是 | 未声明 | 同 Anthropic；`InvokeModelWithResponseStream` | [B1] |
| Bedrock（Converse） | 否 | 否 | — | — | — | [B1] |
| Bedrock（OpenAI `/openai/v1/responses`） | 未发现 | 未发现 | — | — | SSE（Responses 兼容） | [B2] |
| Vertex AI（Gemini） | 否 | 否 | — | — | — | [G10] |
| Vertex AI（Claude，Google managed） | 否 | 是（两套 beta） | 是 | 未声明 | `:streamRawPredict` | [A2][A3][A10] |
| Microsoft Foundry（Claude） | 否 | 是（Anthropic 声明；MS 文档未列） | 是 | 未声明 | 同 Messages API | [A2][A3][F1] |
| OpenRouter | 否 | 否（仅 middle-out 截断） | 否 | — | 由底层模型决定 | [R1][R2] |
| DeepSeek | 否 | 否（`context_management` Not supported） | — | — | — | [D1] |
| Mistral | 否 | 否（Responses 明确 stateless） | — | — | — | [M1][M2] |
| 阿里云百炼 Qwen | 否 | 否（建议应用侧截断/摘要） | — | — | — | [Q1][Q2] |
| Moonshot Kimi | 否 | 否（建议应用侧截断/摘要） | — | — | — | [K1][K2][K3] |
| 智谱 GLM | 否 | 否（仅上下文缓存） | — | — | — | [Z1][Z2] |

## 8. 对 Stravia 的含义

以下区分上游能力与 Stravia 接入契约。Stravia 独立压缩现已切换为 V2：`POST /v1/responses`、`stream: true`、末尾唯一 `compaction_trigger`；不再暴露或回退到 `/v1/responses/compact`。本报告保留的厂商旧端点描述是上游事实，不代表当前 Stravia 支持该入口；仅支持旧端点的上游不因此具有 V2 能力。客户端重建保留窗口，平台不再执行旧的保留消息预算与截断。当前边界见 [ADR-0089](../adr/0089-carry-native-compaction-across-anthropic-and-responses.md)。

### 8.1 可以忠实转发的上游

只有 Anthropic、OpenAI、xAI 三家具备“服务端压缩 + 可回传状态”的完整闭环：

- **Anthropic**：两套机制要分别建模，不能合并。`compaction` 是“一次请求只返回 block、不产生回复”，适合 `/v1/responses/compact` 或 `/v1/messages` 的显式压缩语义；`context_management.edits[compact_20260112]` 是普通轮次内的自动压缩，返回的是“block + 正常回复”，语义上不能当作 standalone compact 结果。block 必须逐字节透传（含 `signature`），Stravia 不得重新序列化或补字段。
- **OpenAI**：`/responses/compact` 与 `context_management`、`compaction_trigger` 是三种不同契约；`CompactedResponse.output` 必须整体作为下一次 input，不能只取 compaction item。`compaction_trigger` 必须位于 input 末尾，转发时不能重排 items。
- **xAI**：形状与 OpenAI 兼容但**不是同一个密钥体系**；`encrypted_content` 只对 xAI 有意义，绝不能把 OpenAI 的 compaction item 发给 xAI，反之亦然。

### 8.2 没有原生能力、必须返回 unsupported 的上游

以下上游**无法忠实转发**远端压缩请求，Stravia 必须明确返回 unsupported，而不是本地生成摘要：

- **Google Gemini（模型级 `generateContent` / Interactions）**：没有任何压缩参数或端点；Managed Agents 的自动压缩是 agent 运行时内部行为，不能被外部 compact 请求驱动，也没有可回放结果；Live API 的 `contextWindowCompression` 只作用于实时会话。**结论：对 `/v1/responses/compact` 类请求一律 unsupported。**
- **OpenRouter**：只有 middle-out **截断**（丢中间消息），既不保留语义，也不返回可回放状态；把它当 compaction 会静默改变对话内容，属于不可接受的语义降级。
- **DeepSeek / Mistral / 阿里云百炼 / Moonshot / 智谱 GLM**：官方文档只有“应用侧截断或摘要”的建议与上下文缓存；服务端没有压缩操作。
- **Amazon Bedrock Converse、Bedrock OpenAI 兼容路由**：Claude 的压缩能力只在 InvokeModel / `/anthropic` 路由上；走 Converse 或 `/openai/v1/responses` 的请求不能转发压缩参数。
- **Vertex AI 上的 Gemini**：同 Gemini API，无压缩能力。

现有代码已经按这个方向落地：`backend/crates/stravia-core/src/model_turn/live.rs:795-804` 在 `native_compaction_requested` 时把上游 `vendor_operation_unsupported` 映射为**终止性**错误 `compaction_unsupported`（“Selected Target does not support native compaction”），不会回退到本地摘要；`live.rs:1435-1446` 在压缩状态解析出的 `CompactionTarget` 与当前 Target 绑定不一致时返回 `compaction_target_mismatch`（“Native compaction state is not compatible with this Target binding”），该次尝试可改投到状态所绑定的 Target。这与本文的结论一致：**远端压缩是上游持有的不透明状态，Stravia 只能转发，不能合成；压缩状态只能回到产生它的 Target，不能换一个上游重放。**

### 8.3 平台/模型 gating 建议

1. **按“机制 × 平台 × 模型”三元组做能力声明**，不要按 vendor 一刀切：Anthropic 的 on-demand 与 threshold 平台表不同（Bedrock 只在 threshold 表里，且 AWS 文档与 Anthropic 表对 on-demand 的 Bedrock 支持存在矛盾）；Bedrock 的 `compact-2026-01-12` 只覆盖 Sonnet 4.6 / Opus 4.6，`compact-2026-09-04` 需逐模型卡确认。
2. **beta header 是硬门槛**：Anthropic 漏 header 会在压缩请求和携带 block 的请求上分别报不同的 400；Stravia 必须在携带 block 的每一跳都保留 header。
3. **计费口径**：Anthropic 的 compaction 用量只在 `usage.iterations`；OpenAI 的压缩 item 在 `output` 里；xAI 的 `usage.dropped_message_count` 只是统计字段。Stravia 的 usage 结算不能把顶层 `input_tokens`/`output_tokens` 当作全部成本。
4. **错误可诊断性**：Anthropic 的 529 `compaction_unavailable`、Bedrock 的 529 签名/分类器失败、OpenAI 的 compaction 事件缺失，都需要保留原始错误体，不能折叠成通用 5xx。

## 9. 无法确认项

1. **跨厂商回放**：Anthropic、OpenAI、xAI 都没有声明压缩块可跨平台/跨厂商使用；Anthropic 的 `signature` 校验方、OpenAI 的加密密钥归属均未公开。本文按“不可跨厂商”处理，但这是**推断**，不是文档结论。
2. **Anthropic on-demand 在 Bedrock 的可用性**：Anthropic 的 `supportedPlatforms` 未列 Bedrock，AWS 文档明确写可用（InvokeModel/InvokeModelWithResponseStream/CountTokens）。两家一手来源冲突，未消解。
3. **Microsoft Foundry 的 compaction**：Anthropic 声明 beta 支持，Microsoft Learn 的 Claude 能力表只列 context editing，没有 compaction 条目。
4. **Vertex AI（Google managed）的 beta header 传递方式**：Anthropic 声明支持，但 Google managed 接入页没有说明 `anthropic-beta` 如何随 `:rawPredict` 请求发送。
5. **Codex 后端的 `/responses/compact`**：`openai/codex` 客户端已不调用该端点，后端无公开 reference。2026-11-02 用 ChatGPT OAuth 账号实测，`POST https://chatgpt.com/backend-api/codex/responses/compact` 返回 `404 {"detail":"Not Found"}`，而同账号的普通 `/responses` 正常；`openai/codex` 的 issue #38323、#38706 记录了客户端遇到同样的 404。结论：该后端的独立压缩只能走 V2（流式 `/responses` + 末尾 `compaction_trigger`），Stravia 的 Codex 插件已按此实现并用同一账号实测通过（见 §6.9），不再调用 `/responses/compact`，也没有回退到旧端点或本地摘要。仍未验证：①WebSocket 传输下的同类请求；②官方客户端随请求发送的 `x-codex-turn-metadata`（`request_kind: "compaction"`）是否会影响后端行为——当前未发送且请求被接受。使用 API Key 的 OpenAI 官方 `/v1/responses/compact` 未在本机实测。
6. **xAI compact 的流式与并发限制**：reference 未列 `stream`，也未给速率/并发配额；只能确认“每次调用最多一次压缩”。
7. **Gemini Managed Agents 压缩的触发精度与可观测性**：文档只给“约 135k tokens”，没有精确阈值、关闭开关或返回块；不同 agent 版本是否一致未说明。
8. **OpenAI `context_management` 的模型覆盖矩阵**：指南给了用法，但没有一张“哪些模型支持 compact_threshold”的权威表；只能按具体模型页与实测判断。

## 官方来源索引

**Anthropic**

- [A1] Compaction overview — https://platform.claude.com/docs/en/build-with-claude/compaction
- [A2] Compaction on demand — https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand
- [A3] Compaction at a token threshold — https://platform.claude.com/docs/en/build-with-claude/compaction-threshold
- [A4] Compaction in the background — https://platform.claude.com/docs/en/build-with-claude/compaction-background
- [A5] Compaction that keeps recent turns — https://platform.claude.com/docs/en/build-with-claude/compaction-keep-recent-turns
- [A6] Compaction and preserved thinking — https://platform.claude.com/docs/en/build-with-claude/compaction-thinking-blocks
- [A7] Create a Message (beta) — https://platform.claude.com/docs/en/api/beta/messages/create
- [A8] Claude Platform on AWS — https://platform.claude.com/docs/en/build-with-claude/claude-platform-on-aws
- [A9] Claude Models on Google Cloud（overview）— https://platform.claude.com/docs/en/build-with-claude/claude-models-on-google-cloud/overview
- [A10] Claude Models on Google Cloud（Google managed）— https://platform.claude.com/docs/en/build-with-claude/claude-models-on-google-cloud/google-managed
- [A11] Claude in Amazon Bedrock — https://platform.claude.com/docs/en/build-with-claude/claude-in-amazon-bedrock

**Amazon Bedrock**

- [B1] Compaction — https://docs.aws.amazon.com/bedrock/latest/userguide/claude-messages-compaction.html
- [B2] Responses API — https://docs.aws.amazon.com/bedrock/latest/userguide/inference-responses-api.html

**OpenAI**

- [O1] Compaction guide — https://developers.openai.com/api/docs/guides/compaction
- [O2] Compact conversation（API reference）— https://developers.openai.com/api/reference/resources/responses/methods/compact
- [O3] Create a response（API reference）— https://developers.openai.com/api/reference/resources/responses/methods/create
- [O4] Streaming events — https://developers.openai.com/api/reference/resources/responses/streaming-events
- [O5] Conversation state — https://developers.openai.com/api/docs/guides/conversation-state
- [O6] openai/codex `3026a8e`：`codex-rs/core/src/compact_remote_v2_attempt.rs`（`input.push(ResponseItem::CompactionTrigger {})`，第 177 行）— https://github.com/openai/codex/blob/3026a8e54e0f48c4d8074cd700f35b0697abc329/codex-rs/core/src/compact_remote_v2_attempt.rs#L177 ；`compact_remote_v2.rs`（`run_remote_compaction_request_v2`，第 286 行）— https://github.com/openai/codex/blob/3026a8e54e0f48c4d8074cd700f35b0697abc329/codex-rs/core/src/compact_remote_v2.rs#L286

**Google**

- [G1] `models.generateContent` — https://ai.google.dev/api/generate-content
- [G2] Interactions API reference — https://ai.google.dev/api/interactions-api
- [G3] Interactions API overview — https://ai.google.dev/gemini-api/docs/interactions-overview
- [G4] Managed Agents quickstart — https://ai.google.dev/gemini-api/docs/managed-agents-quickstart
- [G5] Antigravity agent — https://ai.google.dev/gemini-api/docs/antigravity-agent
- [G6] Live API session management — https://ai.google.dev/gemini-api/docs/live-api/session-management
- [G7] Live API reference（`ContextWindowCompressionConfig`）— https://ai.google.dev/api/live
- [G8] Context caching — https://ai.google.dev/gemini-api/docs/caching
- [G9] `models.countTokens` — https://ai.google.dev/api/tokens
- [G10] Vertex AI `generateContent` — https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference

**xAI**

- [X1] Context Compaction — https://docs.x.ai/developers/advanced-api-usage/context-compaction
- [X2] Responses API reference — https://docs.x.ai/developers/rest-api-reference/inference/responses

**Microsoft Foundry**

- [F1] Claude models in Microsoft Foundry — https://learn.microsoft.com/en-us/azure/foundry/foundry-models/concepts/claude-models

**OpenRouter**

- [R1] Message transforms（context compression）— https://openrouter.ai/docs/guides/features/message-transforms
- [R2] Plugins — https://openrouter.ai/docs/guides/features/plugins
- [R3] Compact Long Agent Sessions with ScaleDown Summarize — https://openrouter.ai/docs/cookbook/building-agents/compact-agent-sessions-with-scaledown

**DeepSeek**

- [D1] Using the Responses API — https://api-docs.deepseek.com/guides/responses_api

**Mistral**

- [M1] Responses API — https://docs.mistral.ai/studio/conversations/responses
- [M2] Agents & Conversations — https://docs.mistral.ai/studio/agents/agents-api

**阿里云百炼**

- [Q1] Multi-turn conversations — https://docs.modelstudio.console.alibabacloud.com/en/model-studio/multi-round-conversation
- [Q2] Qwen API via OpenAI Responses — https://docs.modelstudio.console.alibabacloud.com/en/model-studio/qwen-api-via-openai-responses

**Moonshot Kimi**

- [K1] 配置多轮对话参数 — https://platform.kimi.com/docs/guide/engage-in-multi-turn-conversations-using-kimi-api
- [K2] Messages API — https://platform.kimi.com/docs/api/messages
- [K3] Responses API — https://platform.kimi.com/docs/api/responses

**智谱 GLM**

- [Z1] 上下文缓存 — https://docs.bigmodel.cn/cn/guide/capabilities/cache
- [Z2] 对话补全 API — https://docs.bigmodel.cn/api-reference/模型-api/对话补全
