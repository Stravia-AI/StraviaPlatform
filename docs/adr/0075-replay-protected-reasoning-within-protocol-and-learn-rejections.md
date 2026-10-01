---
status: accepted
---

# 同协议内乐观回放受保护推理，并记住上游拒绝

本决策取代 [ADR-0028](0028-persist-hidden-history-behind-markers.md) 中“证明属于其它作用域时剥离”的部分，以及 [Vendor Wasm 插件设计](../design/vendor-plugins.md) 中“受保护推理每级恢复都消耗恢复预算”的约定；ADR-0028 的来源记录、Marker 持久化、可读推理的 codec 表示和两级剥离顺序保持不变。

Thinking Replay 只按兼容性决定受保护载荷（签名、`encrypted_content`、redacted block）能否回放，不以防泄露为由剥离：密文是签发方加密的不透明载荷，发给错误的上游不构成可接受范围外的披露。只有能确定上游必然拒绝，或拒绝后无法可靠识别时，才提前剥离。其余情况一律保留，由插件把上游拒绝分类为 `protected-reasoning-rejected`，再由宿主分级剥离并重放。

判定规则如下：

- 出口协议不同时剥离。跨厂商签名不可能有效。
- 出口协议要求模型绑定（目前仅 Gemini）且模型不同时剥离。
- 当前签发作用域已经拒绝过该载荷时剥离。
- 签发作用域相同时保留。
- 同协议下仅部署地址或凭据身份不同（换账户、换 org、换中转）时保留，按来源不明处理。
- 没有来源记录时保留，按来源不明处理。

分级重放不再计入 Target 的连续失败，也不消耗 Target 重试预算。上游因载荷拒绝请求，说明 Target 本身健康；这类恢复只受两级剥离上限约束，每一级都必须实际移除载荷。

宿主在内存中保存拒绝记忆，键为当前签发作用域和载荷摘要。剥离后的重放不再被拒时，记录该级剥离掉的载荷；此后回放给同一签发作用域时直接剥离，不再重复“先被拒、再恢复”的往返。

## 表示边界与工具历史导入

来源判定沿用上面的规则，不引入第二套 IR。已知跨协议交付时，既有一对一 History Marker 保存原始思考块及可信 `ThinkingSource`；公开预览不暴露外来原生签名。客户端保持同一个模型名、内部 Target 切换或客户端修改前缀产生新根时，Marker 仍能恢复签发协议和 Gemini 模型约束。原生同协议输出保留其兼容载体；来源不明的历史仍乐观处理。

`Thinking.signature`、Responses `Reasoning.encrypted_content` 和 redacted 数据不是可互换的协议字段。出口 codec 只保留该协议支持的原生载荷；不支持的受保护数据不得伪装成其它协议的密文、签名或可读正文。可读段落及普通文本顺序、工具调用与结果的关联保持不变。Responses 上游 REQUEST 的 reasoning `content` 为空，完整可读正文放到相邻 assistant `output_text`；这不改变客户端 OUTPUT 的原生 reasoning `content`。

Gemini 原生 `functionCall.thoughtSignature` 必须回到同一个 functionCall part。既有空的签名 Thinking 载体只与紧邻调用配对，普通文本、其它 part 或角色边界阻止配对；同步请求、响应与跨批次流事件采用相同规则。无签名的可读 thought part 仍保持 Gemini 原生表示。

Google 的 [thought signatures 文档](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures)规定：Gemini 3 当前工具轮每一步的第一个 functionCall 必须带签名；迁移其它模型的工具历史时可使用 `skip_thought_signature_validator` 跳过这项校验。本平台仅在确认的官方 HTTPS `generativelanguage.googleapis.com:443`、`/v1beta/models/...:generateContent` 或 `:streamGenerateContent`、Gemini 3 / 数字次版本 3.x 模型请求中，对当前轮缺失、null 或空的首个调用签名使用此控制标记。并行其它调用、早先工具轮及真实非空签名不改写。它只存在于已编码的上游 REQUEST，不作为真实签名写入权威历史、来源或客户端输出，也不能恢复已丢失的私有推理。

不对 Vertex、改写 base URL 的自定义端点、未知 latest alias、其它模型代际或未确认的 API 路径推断支持此机制；这些端点保持原行为，仍可能拒绝当前轮无签名工具历史。Google 不建议手工注入 functionCall；上述例外仅用于必要的工具历史迁移，不改变工具执行权限或访问控制。

## Considered Options

- **保持现状，签发作用域不同即剥离。** 这样做安全但过度剥离。Claude 签名在账户、平台之间通用，OpenAI 只按 organization 绑定，现状会无谓丢失推理链。
- **由插件经 WIT 声明各厂商签名的绑定维度，并上报 organization 等签发事实。** 这是最精确的做法，但需要破坏性升级插件契约，所有插件都要声明和维护厂商知识。有了拒绝分类和拒绝记忆，这类情况只多付一次请求，不值得这套复杂度。
- **完全不提前剥离。** 跨协议必然被拒，每次都会白白多一次请求；Gemini 换模型后的拒绝形态也没有核实。

## Consequences

- 同协议下切换账户、org 或中转后，每个签发作用域的首轮最多多出两次上游请求，都发生在任何输出之前。之后由拒绝记忆避免重复。
- 拒绝记忆是可丢失的纯性能状态：进程重启后清空，多实例之间不共享，不需要 schema 变更。丢失只会让下一次请求重新经历一次恢复。
- 新插件只要正确分类受保护推理拒绝，就能获得这套兼容行为，无需声明签名作用域。分类错误会让拒绝以普通错误返回；正因如此，拒绝无法可靠识别的场景仍然提前剥离。
- 权威历史和 History Marker payload 仍不改写。客户端回到兼容来源时，仍可回放原密文。
- 两个协议即使实际同源也按不同协议处理，例如 Bedrock 与 Anthropic 官方的 Claude 签名，会提前剥离。不为此增加协议族映射。
