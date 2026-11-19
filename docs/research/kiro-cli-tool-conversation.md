# Kiro CLI 3.4.0 工具与多轮请求证据

本文保存外部 ACP 的对照实验；插件的主基准是[官方交互式 TUI 的真实 HTTP 请求](kiro-cli-request-alignment.md#交互式-tui-实测)。

本地实验日期：2026-11-18。官方 3.4.0 的 V3 KAS（ACP stdio，KAS 0.66.111）以合成 API key、临时 HOME 和只含一个合成文本文件的临时工作区运行。父进程和 KAS 在独立用户及网络 namespace 中，只有 loopback，runtime 与 management endpoint 均指向本地 HTTP stub；未读取真实账户、用户目录或连接公网。模型目录和全部上游响应由 stub 构造，这不是线上服务行为的证明。没有替换或仿造 CLI 的内置 prompt、工具或工作区逻辑。

ACP 客户端新建 session、选本地合成目录返回的 `qwen3-coder-next`，发送第一条 prompt。stub 用 AWS EventStream 二进制帧依次返回：

```json
{"assistantResponseEvent":{"content":"Checking the file."}}
{"toolUseEvent":{"toolUseId":"synthetic-tool-1","name":"read_file","input":"{\"path\":\"/tmp/kiro-tool-wire/work/input.txt\"}"}}
{"toolUseEvent":{"toolUseId":"synthetic-tool-1","name":"read_file","stop":true}}
{"metadataEvent":{"stopReason":"TOOL_USE"}}
```

KAS 产生 ACP tool call，并实际执行其内置 `read_file` 读取该合成文件。stub 对工具回执后的生成返回 `assistantResponseEvent` 内容 `Synthetic file answer.` 和 `metadataEvent` 的 `END_TURN`。ACP 首个 prompt 正常结束；随后同一 ACP session 发送 `Next synthetic turn.`，stub 返回 `Synthetic next turn answer.` 和 `END_TURN`，第二个 prompt 也正常结束。所有输出和停止原因均由 stub 合成，不是模型生成。

## 请求结构

以下是三次实际 KAS 请求的关键字段，重排并以 `<S>`、`<C1>`、`<C2>` 代替本地生成的标识；`<initial>` 是 CLI 自己加入的初始 user/assistant 两条消息，未在此复制，`<context>` 是 CLI 自己加入的 session context；`<tools>` 代表本次观察到的 14 个内置工具定义，未复制。`origin` 均为 `KIRO_CLI`、`modelId` 均为 `qwen3-coder-next`，顶层还有 `agentMode: "vibe"`，state 还有 `chatTriggerType: "MANUAL"` 和 `agentTaskType: "vibe"`。省略这些重复字段不表示实际请求中不存在。

```text
请求 1
  conversationId = rootConversationId = <S>; agentContinuationId = <C1>
  history = [<initial>]
  currentMessage.userInputMessage = {content: "<context>\n\nRead the synthetic input file.", userInputMessageContext: {tools: <tools>}}

请求 2（工具回执）
  conversationId = rootConversationId = <S>; agentContinuationId = <C1>
  history = [<initial>,
    userInputMessage {content: "<context>\n\nRead the synthetic input file."},
    assistantResponseMessage {content: "Checking the file.", toolUses: [{toolUseId: "synthetic-tool-1", name: "read_file", input: {path: "/tmp/kiro-tool-wire/work/input.txt"}}]}]
  currentMessage.userInputMessage = {content: "", userInputMessageContext: {
    toolResults: [{toolUseId: "synthetic-tool-1", content: [{text: "<file name=\"/tmp/kiro-tool-wire/work/input.txt\" language=\"plaintext\" >\n<content>\nsynthetic file contents\n\n</content>\n</file>"}], status: "success"}], tools: <tools>}}

请求 3（下一次独立 ACP prompt）
  conversationId = rootConversationId = <S>; agentContinuationId = <C2>
  history = [<initial>, 第一次 user 消息, 上述 assistant toolUses 消息,
    userInputMessage {content: "", userInputMessageContext: {toolResults: [上述结果]}},
    assistantResponseMessage {content: "Synthetic file answer."}]
  currentMessage.userInputMessage = {content: "Next synthetic turn.", userInputMessageContext: {tools: <tools>}}
```

本次 `<S>` 是 ACP `session/new` 返回的 `sess_` 会话 ID；`<C1>`、`<C2>` 是不同 UUID，均不等于 `<S>`。工具回执续腿保留 `<C1>`，下一独立 prompt 使用 `<C2>`。stub 的响应 header `x-amzn-kiro-conversation-id` 刻意设为另一合成值，观察到后续请求仍使用 `<S>`；不能据此推断所有服务端会话处理。此前相同实验若省略 metadata stopReason，KAS 在文本后发出内容为 “The previous response was interrupted. Continue from the recorded response; do not repeat completed work.” 的额外续请求；该观察仅说明本地构造的无终止事件流如何被 KAS 处理，不代表真实上游一定如此。

## 重新登录后的真实服务验证

[线上 ACP 记录](kiro-cli-request-alignment.md#对照340-重新登录后的外部-acp-线上回合)验证了同一行为：所选 `qwen3-coder-next` 完成 `read_file`，使用匹配的 ID 提交结果；下一轮不读文件也正确复述测试串。工具续接和下一用户输入的 continuation ID 作用域与本地样本一致。这一结果适用于这次 CLI 登录调用，不代表 Stravia API key 已在公网验证。

与 Stravia 的对应：已观察的工具调用与回执结构、结果移入 history 的行为由 request/stream 回归覆盖；宿主按用户输入提供的 `user_prompt_id` 供 `agentContinuationId` 使用，工具续接复用、独立输入改变，与本次 ID 作用域一致。Stravia 不复制 KAS 的内置 prompt、工具或工作区。合成流无法证明真实服务认证、真实模型输出或所有错误及分片行为。
