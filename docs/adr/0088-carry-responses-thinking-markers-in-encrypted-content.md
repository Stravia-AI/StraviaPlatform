---
status: accepted
---

# Responses 客户端的 Thinking Marker 经 `encrypted_content` 回传

本决策修订 [ADR-0028](0028-persist-hidden-history-behind-markers.md) 与 [ADR-0033](0033-stream-text-and-project-post-text-thinking.md) 中 Responses 出口的 Thinking Marker 载体：Marker 不再追加到 reasoning `content` 的末段，改放 `encrypted_content`。其它出口协议的载体、一对一关系、Principal 作用域和“删除即编辑”的规则不变。

## 背景

Responses 客户端在无状态回放（`store=false` 的 Codex HTTP、AI SDK 等）时重建 reasoning item：只回传 `id`、`summary` 与 `encrypted_content`，丢弃 `content`。真实 OpenAI 的 REQUEST 也不接受 reasoning `content`。把 Marker 写在 `content` 中，意味着这类客户端会静默丢弃它，上游随后看不到 Anthropic 的 `signature` 或 `redacted_thinking.data`，带工具调用的轮次因此可能被拒。

`encrypted_content` 是 Codex 与 AI SDK（`store=false`）这两类已核对的客户端重建 reasoning item 时都会原样回传、同时不向用户显示的字段。

## 规则

- 对 Responses 出口，任何需要 Marker 的受保护思考（Anthropic `thinking`＋`signature`、`redacted_thinking`、Gemini 带 `thoughtSignature` 的 thought，以及其它非原生来源）都投影为 reasoning item：可读预览留在 `content`（或 `summary`），`encrypted_content` 为 Stravia 令牌 `<!--sh:REF-->`。令牌与既有 History Marker 同语法、同 `REF`，不含来源签名或密文。
- 投影位于共享 IR 层（引擎的客户端投影与 History Marker 解析器），只依赖通用的 `Thinking`／`Reasoning`／`RedactedThinking`，不针对任一上游协议。
- 没有可读预览的块（如 `redacted_thinking`）产生只有令牌的 reasoning item，占用自己的 `output_index` 与 `rs_` 前缀 id。
- 流式输出：预览先走既有 reasoning delta，块结束时以带令牌的 reasoning `output_item.done` 封口；没有预览的块先打开空的 reasoning item 再封口。同步响应、`response.completed.output` 与 Generation Chain 的客户端历史使用同一形态。
- 解析请求时，`encrypted_content` 以私有前缀开头的 reasoning item 按 Marker 处理：恢复为权威思考块；令牌无法解析、不属于当前 Principal 或引用不存在时清除，绝不转发给上游。真实密文不以该前缀开头，不受影响。
- 令牌不是上游载荷：它不进入受保护载荷摘要、来源清单和 ADR-0080 的拒绝记忆。
- 旧历史中位于 `content` 的 Marker 继续按原规则解析；新输出只使用 `encrypted_content`，不双写。

ADR-0075 与 `docs/design/open-responses.md` 关于 `Thinking.signature` 不得伪装为 `encrypted_content` 的约束保持：令牌是指向服务端私有历史的指针，签名、密文和 redacted 数据本身从不离开网关。

## Considered Options

- **保持 `content` 载体。** 实现最少，但无状态客户端丢失 Marker，受保护思考无法恢复。
- **放入 `summary`。** 客户端会回传，但改变“summary 为空”的输出契约，需要新的流事件，且 AI SDK 等会把它显示给用户。
- **以 item `id` 作为载体。** 与 Marker 一对一并检测预览编辑的设计冲突。
- **在 `encrypted_content` 内封入自包含的加密载荷。** 不依赖服务端存储，但扩大了密钥管理与泄露面。

## Consequences

- Codex、AI SDK 等无状态客户端可以恢复 Anthropic、Gemini 等来源的受保护思考。
- 客户端若自行清空 `encrypted_content`，等同于删除 Marker，按 ADR-0028 视为有意的历史编辑。
- 原生 Responses 上游返回的真实密文行为不变。
- 客户端不再能在 `content` 中看到 Marker 文本。
