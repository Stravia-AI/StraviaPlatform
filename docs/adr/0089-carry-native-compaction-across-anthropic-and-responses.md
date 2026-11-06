---
status: accepted
---

# 跨 Anthropic 与 Responses 承载原生压缩

Remote Compaction Request 由当前选中 Target 的上游执行，结果经过规范 IR，再编码为客户端协议。Stravia 不生成本地摘要，不为完成压缩重试或切换 Target。上游机制与覆盖范围见 [研究报告](../research/upstream-remote-compaction.md)；厂商仍提供的旧压缩端点不等于 Stravia 继续提供该入口。

## 契约与取舍

- **入口**：Responses 客户端使用 `POST /v1/responses`、`stream: true`，在 `input` 末尾放置恰好一个 `compaction_trigger`。入口解码为 `ContentBlock::CompactionTrigger` 后分类为 `ModelTurnPurpose::Compact`，不创建 Generation，不运行 Agent 循环。`POST /v1/responses/compact` 已删除，返回 404；不保留 V1 handler、请求白名单或回退。Anthropic `/v1/messages` 的非空 `compaction` 对象同样转为规范摘要控制与末尾触发器，`compaction: null` 视为未请求。
- **交付**：Responses V2 返回普通 Response SSE，包括唯一原生 compaction item 与 `response.completed`，不返回 `response.compaction` 或替客户端拼接保留窗口。客户端自行按自身规则重建窗口。Anthropic 入口仍返回一个原生 `compaction` 块与 `stop_reason: "compaction"`，是否使用 SSE 由入口 `stream` 决定。WebSocket 不提供独立压缩；收到末尾触发器明确返回 `unsupported_feature`，不能静默执行普通推理。普通 WebSocket 请求的 inline `context_management` 不受此限制。
- **发布确认**：独立压缩在成功校验并完成状态登记后才开始交付；JSON 与 SSE 都以完整 HTTP 响应体交付结束为发布确认边界，中途断开保持未确认，不能仅因交付了状态事件就提前确认整个独立结果。
- **IR**：摘要任务指令属于 `CompactionControl.instructions`；会话指令属于 `AiRequest.instructions`，二者不可混用。`NativeCompactionResponse.items` 是状态登记与协议输出的来源；`wire` 只保留上游真实资源的身份、时间与原生用量扩展，不用于绕过输出编码。`ContentBlock::Compaction.summary` 存在即 Anthropic 来源：`encrypted_content` 存原生签名，`summary` 存可读摘要，二者都进入原生状态指纹。Anthropic 条目 ID 由签名哈希生成，登记与回放保持一致。
- **默认指令**：客户端未提供会话指令时，独立压缩补入结构化交接摘要提示词，明确只总结历史、不继续任务、不调用工具，并保留目标、约束、进展、决策、待答问题与后续动作。显式会话指令保持原样。Anthropic 未提供 `compaction.instructions` 时使用同一缺省摘要任务指令；显式摘要指令保持原样。发往 Responses 时，若存在独立摘要指令，将其作为标记为 `Compaction instructions` 的摘要任务附在会话指令后，而不是覆盖会话指令。
- **Responses 出口**：所有 Responses 独立压缩共用普通流式 `/responses` 请求与共享 SSE 解码器，末尾只有一个触发器，`stream=true`、`store=false`；保留正常编码支持的 tools、reasoning 与其他请求控制，不使用 V1 字段白名单。协议可承载但上游能力未知时由上游裁决，不回退到 `/responses/compact`。仅提供旧端点的上游不因此获得 V2 能力。
- **流式校验**：每个 SSE payload 只做一次 JSON 解码，再由标准 Responses 流状态机生成 typed IR。必须有成功终态、恰好一个带非空状态的 compaction item，不能有未完成条目或其他输出。完整状态可以只出现在 `response.output_item.done`，终态 `output` 允许为空；若终态重复同一索引的状态，必须一致。错误、拒绝、截断、重复状态与不完整终态不能登记为成功，原生错误保留既有错误分类与错误体。
- **Anthropic 出口**：保留 model、system、tools、thinking 与 effort，发送 `stream: true`、`compaction: {"type": "summarize", "instructions": ...}`，合并 `anthropic-beta: compact-2026-09-04`。仅剥离互斥的 `stop_sequences`、`tool_choice`、`output_config.format`、`output_config.task_budget.remaining` 和 `context_management`。必须收到 `message_stop` 且 `stop_reason` 为 `compaction`；完整签名块、未知字段和 `usage.iterations` 经 IR 保留。原生用量按 compaction iteration 结算，不把顶层零值误计为零成本。
- **无状态信封**：Anthropic 的完整原生块封进 Responses `encrypted_content`；Responses 状态封进 Anthropic 块的 `signature`，`content` 为固定占位文本。信封使用带版本前缀的 base64url JSON，不新增持久化表。解码恢复原生 IR，只有目标协议不同才封装；编码必须以当前规范状态为准，同时保留签名块的未知字段，不能输出裸 Anthropic signature 作为 Responses 密文。
- **回放**：状态的来源、Principal、有效期与 Target 绑定继续按既有规则校验。同一 Anthropic Target 回放原生块，保持签名与内容以及后续 thinking、text、tool_use 的相对顺序，并携带压缩 beta；最新块替换此前的整个窗口。Anthropic 可读摘要在不能保留其原生协议或 Target 绑定时可按既有规则降级为历史摘要文本；无摘要的 Responses 不透明状态仍不能跨 Target 回放。这延续 ADR-0047 的不透明状态约束及本决定对 Anthropic 可读摘要的明确例外。
- **凭据保护**：原生 compaction 载荷不还原凭据占位符，避免破坏签名；回放仍使用上游原样签名与内容，不借摘要还原泄露凭据。
- **能力投影**：管理读模型时派生只读字段 `supports_remote_compaction`，不持久化。至少存在一个 Target，且全部配置的 Target（包括已禁用者）都具备有效 Provider Model、当前有效插件与通道的 `Compact` 能力，并由同一插件的纯协议选择阶段选中 Open Responses 或 Anthropic Messages，才标注支持。复用实际执行的准入与协议选择，不凭厂商名、模型名或保存的协议提示猜测，不请求真实上游；缺失、不可用或选择失败按不支持处理。这是当前声明能力，不保证具体上游最终接受压缩请求，也不改变运行时未知能力交由上游裁决的行为。
- **客户端配置**：`connect_client_apply` 按上述后端投影，只在合格的 omp 模型条目内写入 `remoteCompaction: { enabled: true, v2StreamingEnabled: true }`，不设置 provider 级压缩默认值；混合 Target 或能力未知的模型不写这些键。模型级控制遵循 [OMP 配置契约](https://github.com/can1357/oh-my-pi/blob/main/docs/models.md)。重新接入时替换该 provider 及其模型条目的旧压缩配置；Pi 配置不写入这些 omp 专用字段。Stravia 不执行客户端本地摘要回退；客户端自己的后续方法仍由客户端配置决定。
- **Codex 插件**：Compact 与普通推理共用原生 `/responses` 请求准备、headers 和会话身份投影，但独立压缩固定走 HTTP SSE；不保留 V1 保留消息预算、截断器或 `response.compaction` 包装。非 2xx 和流内上游错误保留现有分类；失败不触发恢复或重放。更新后需重新构建并导入独立 Codex 插件。
- **Claude Code 插件**：封闭字段集收窄前消费规范摘要控制，之后写入原生 `compaction`。独立压缩与携带状态的推理禁用 thread、diagnostics 与线程 beta，发送完整窗口；没有压缩块的普通会话沿用线程机制。请求带压缩控制或回放块时声明压缩 beta，缓存断点不标注 compaction 块。更新后需重新构建并导入独立 Claude Code 插件。

本次验证采用实际构建的服务端与 Wasm 组件、隔离 loopback 上游；它不替代 Anthropic 或 ChatGPT 线上账号验证。此前记录的真实账号结论见研究报告，不据此声称本次再次验证了线上上游。
