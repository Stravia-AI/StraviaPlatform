---
status: accepted
---

# Own Protocol Conversion behind a pair-bound canonical interface

> 部分被 [`ADR-0056`](0056-grow-protocols-by-unique-wire.md) 取代：协议集合固定为四条的约束不再适用。Protocol 改为按 wire contract 去重，只有 wire 确实不同才新增。

> 部分被 [`ADR-0011`](0011-own-open-responses-as-a-dated-protocol.md) 取代：pair-bound conversion seam、canonical ownership 与 representability gate 继续有效；最终 canonical shape 改为 ordered `AiItem` Graph 和 lifecycle-complete stream events。

Stravia 将 Protocol Conversion 收进 crate-private deep module：`ProtocolTransform::bind(ingress, egress)` 产生绑定协议对的 `ProtocolPair`，只暴露 request decode/encode、response decode/encode 与 stateful stream session；`AiRequest`、`AiResponse`、`AiStreamDelta` 保持唯一 canonical IR。转换以客户端与目标协议的兼容性为第一优先级：尽量保留目标可表达的内容与控制，不因目标不支持的可选处理提示阻断合法请求；必要内容与执行语义仍由 representability gate 校验。HookRuntime 继续拥有 Hook lifecycle，Vendor adapter 继续拥有 auth、base URL、HTTP、routing 与 deadline。

## Considered options

- 直接依赖 agentgateway `agent-llm`：其 workspace `version = "0.0.0"`、`publish = false`，conversion 使用 pairwise wire types，stream 绑定 Axum `Body`，无法形成稳定、无 transport 的 library seam。
- 直接依赖 AISIX：`Bridge` 同时拥有 transformation、HTTP、auth、deadline 与 stream，`Guardrail` 只能返回 verdict，不能承载 Stravia Hook 的 canonical mutation；内部 wire modules 也不承诺 public-SDK 稳定性。
- 直接依赖 `anyllm_translate`：它虽是已发布、默认无 IO 的纯 crate，但以 Anthropic Messages 为 hub，并会丢弃 multi-candidate、部分 reasoning signature、cache usage 与 Responses/Gemini thinking；这会引入第二 IR 和 silent loss。
- 完整 vendor `anyllm_translate` 后逐步重构：便于追踪上游 diff，但会在迁移期保留双 IR、双 wire types 和已知有损路径。
- 最小 generic operation algebra：external interface 最窄，但会把 Inference Run、Tool Continuation 和 Hook lifecycle 拉进 transformation seam。
- 通用 compile-plan interface：扩展性最高，但会为当前封闭协议集合新增 `EndpointKey`、capability/loss vocabulary 和 speculative extension seams。
- 采用的 pair-bound interface：保留 `ProtocolEndpoint` 与 `ProtocolRegistry`，用一个 concrete façade 和 crate-private wire adapters 集中协议差异；caller 只在 canonical value 上运行既有 Hook/Vendor ordering。独立 request/response stage wrappers 未保留，因为它们只转发 `ProtocolPair` 操作、增加 interface 而没有隐藏额外 implementation。

## Consequences

- `anyllm_translate` 0.16.0 release commit `75a5a3a` 仅作为一次性源码 donor。Stravia 选择性移植纯 mapping、stream state-machine 算法与有效 fixtures，保留 MIT attribution；不引入 runtime dependency，不保留其 Anthropic hub、middleware、proxy、model map、随机 ID/time helper，也不维护上游同步 patch。
- OpenAI Chat Completions、Open Responses 2026-04-24、Anthropic Messages、Google Gemini generateContent 在同一次 clean cutover 中迁移。OpenAI embeddings 接入新 unary façade且保持行为不变。旧六类 codec trait factory 已从调用 interface 删除；crate-private wire traits 与 `inventory` registration 仍留在 deep module 内，作为 `ProtocolAdapter` 的实现机制，不形成第二条调用路径。
- `decode_request` 执行 ingress decode；Request Hook 与 Vendor canonical mutation 完成后，`encode_request` 运行 representability gate 并生成 egress body、协议必需 headers 与 relative path。`decode_response` 与 `encode_response` 对称地执行 egress decode、Response/Client Output Hook 与 ingress encode。transport secret、base URL 和 side effects 永不进入 transformation module。
- 可选的 Provider 处理提示以兼容性优先：目标支持时保留，目标不支持时允许省略，不以逐字段无损为由拒绝整个请求。图片分辨率 `detail` 遵循 [oh-my-pi 的 Provider hint 语义](https://github.com/can1357/oh-my-pi/blob/v18.4.4/packages/ai/src/types.ts#L872)：任何不支持该控制的目标协议均忽略入口已接受的全部 `detail` 值（包括 `auto`、`low`、`high`），由目标自行处理图片；Anthropic 与 Gemini 不再因该提示拒绝请求，OpenAI-compatible 与 Open Responses 仍保留该字段。不全局删除 canonical 提示，不增加严格模式或兼容开关。
- 兼容性优先不允许丢弃图片或文本本体、伪造工具入参、破坏 tool correlation、reasoning signature、usage cardinality、multi-candidate、stop reason 或绕过安全校验。此类必要内容、执行语义和未声明 extension 必须可表示或返回 typed `ProtocolLossyRejected`；warning、默认值和 silent drop 不能替代拒绝。same-protocol 路径仍经过 canonical IR，并按现有 `RawEnvelope` / `VendorExtensions` policy 保留允许透传的字段。
- Devin 的明确例外：为提高请求兼容性，Vendor canonical mutation 可以在 representability gate 前按固定清单静默移除无法表达的可选请求控制，包括工具选择、并行控制、采样、格式和候选数要求，改用 Devin 原生行为。这不授权丢弃消息、媒体、工具结果、安全设置或数据位置约束，也不放宽共享转换校验；具体清单与不再保证的约束见 [Vendor Plugins 设计](../design/vendor-plugins.md#已确认的供应商流程与网络执行)。其他供应商仍遵循各自原有契约。
- stream session 固定 `provider wire decode → canonical vendor normalization → Hook transform → client wire encode` ordering，并拥有 parser、formatter、usage、tool index 与 terminal state。请求阶段可预判的损失在 provider call 前拒绝；Client Output Commit 后首次出现不可表示 delta 时，session 立即生成目标协议 error event 并终止，不静默丢弃、不切换 Target，也不默认缓冲完整跨协议流。
- pair-bound façade 的 direct contract tests 覆盖 native/cross request、same-protocol stream loss bypass、cross request rejection，以及 delivery 的 cross response/stream rejection；另有保留 MIT attribution 的 `anyllm_translate` donor cases 覆盖 system、image、tool turn、response 与 stream stop reason。各 endpoint 的 codec matrix 与 parser microtests 另行覆盖 wire 行为。这些集合互补，但不宣称穷举全部 ingress×egress×stage 组合。删除 deep module 后所有 ingress/provider/delivery wire 路径均应失去合法实现，这是该 module 的 deletion test。
