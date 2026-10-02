---
status: accepted
---

# Persist hidden history behind one-to-one markers

> **部分被取代。**
> - 「Streaming」一节中“普通可见 delta 立即发送”以及 Marker 载体的相关约定，曾被 [ADR-0030](0030-project-history-markers-through-reasoning.md) 取代；ADR-0030 又被 [ADR-0033](0033-stream-text-and-project-post-text-thinking.md) 整体取代，当前以 ADR-0033 为准。
> - 「Target and storage boundaries」一节中“证明属于其它作用域时剥离”的判定，由 [ADR-0075](0075-replay-protected-reasoning-within-protocol-and-learn-rejections.md) 取代：同一出口协议下仅部署或凭据不同的受保护载荷改为保留，只有出口协议不同、协议绑定的模型不同，或已被当前签发作用域拒绝过时才剥离。
>
> 其余约定继续有效，包括 Principal 隔离、一对一 Hidden History Segment、durable execution，以及来源记录、可读推理的 codec 表示和两级剥离顺序。

Stravia 将客户端可见历史与 Provider 有效历史保持为两个视图：客户端投影只用 Principal-scoped History Marker 表示未披露内容，History Marker Store 持久化实际 Hidden History Segment，恢复请求时在 Marker 原位置替换后再发送上游。Marker 不依赖周边上下文匹配；客户端可以修改其他历史。一个 Marker 只对应一个 Platform Tool Execution 的 call/result 对，或一个受保护 Thinking block，禁止聚合多个工具执行或多个 block。

## Client projection

- 所有 ingress 协议都把 Platform Tool call/result 隐藏为 Marker。已知出口协议与客户端协议不同时，可见 reasoning 继续输出，既有一对一 Thinking Marker 保存原始 block 与可信来源，避免客户端把外来签名误认作自己的密文或签名。Marker 不依赖前缀未被编辑或父历史匹配。原生同协议可无损表示时继续使用原生字段；Chat 的 post-text 引用/正文载体策略不因此推广到其它协议。
- Marker 是仅供机器读取的独立 HTML comment，包含不可猜测的短引用，不产生用户可见文案。
- 除删除 Platform Tool 并插入对应 Marker 外，客户端响应的正文、公开 client tools、usage、stop/finish reason、response identity 和协议终态保持原行为。
- 客户端提交的私有 Marker 无法解析、无权访问或已过期时清除该 Marker block，不把私有格式发送给模型。同一请求内相同 Marker 只展开第一次，后续重复块清除；同一 Marker 在不同请求和并发分支中可重复使用。
- 客户端当前提交的公开 client tool calls/results 是权威值，可以增删改排；恢复时隐藏 Platform calls/results 排在公开 calls/results 之前。Marker 不恢复或覆盖其他客户端历史。
- Marker 在所有响应和 stream Hook 之后由客户端投影生成，ID 和结构不允许 Hook 修改。

## Durable execution and rendezvous

- History Marker Store 是 hidden payload 和 Platform Tool Execution 状态的唯一事实源；Generation Chain 只持久化 Marker reference，不复制 hidden payload。Marker 在交付前持久化，交付后按 Generation Chain 保留期发布，分支延长祖先时同步延长引用的 Marker；未发布记录按 pending 保留策略清理。
- 混合 Platform/client tool 轮次并行执行：每个完整 Platform call 建立一个 Marker 和后台 execution，同时把公开 client call 返回客户端。Platform 先完成时 durable 保存并等待客户端；客户端先返回时请求等待 Platform terminal，随后用 call/result 替换 Marker并正常进入上游流程。
- Platform Tool Execution 使用数据库条件更新原子 claim和持久 owner lease；其他实例只等待。进程崩溃或 owner lease 失联时，`running` 转为失败 tool result，向模型说明执行中断并允许模型重新请求；Stravia 不自动接管或重放可能已产生副作用的调用。
- 每个 Platform Tool 在注册元数据声明既有执行上限，未声明时使用全局默认；execution record持久化绝对 deadline。Marker 发布后 execution 独立于原请求和后续 waiter cancellation。后台执行沿用创建时授权，不新增运行中权限复查。
- 后续请求等待 Platform execution时不消耗模型执行的 300 秒期限；rendezvous 完成后重新开始正常执行期限。等待连接被外部关闭只移除该 waiter，不取消共享 execution。
- 后台 execution 保留创建请求 R1 的 RootRequest 上下文，而非继承并发 lease；内部模型发送仍经过上游 RPM Pool，累计等待与冷却额外尝试不因后台化重置。匹配 Marker 的后续客户端请求是独立根请求，按 API Key Root RPM 准入后等待 rendezvous；execution terminal 不释放任何入口额度。此项替换原有继承 Principal Concurrency Limit 名额的约定，见 [ADR-0015 的替代决定](0015-replace-api-key-rate-limits-with-principal-concurrency-admission.md)。
- 只有 Platform Tool、没有 client tool 时，在同一客户端 stream输出 Marker，等待 execution完成后继续下一 Model Turn；不强制客户端创建额外请求。

## Streaming

实时 streaming 是底线，不允许为了 Marker 缓冲整个 Model Turn。普通可见 delta 立即发送；每个尚未分类的 tool index只缓冲到名称可分类，Platform call继续按该 index缓冲到 `ToolCallComplete`。完整 call到达后一次事务持久化 Marker/execution，输出对应 Marker，隐藏该 Platform call的全部 wire delta并开始执行；公开 text、reasoning 和 client-tool delta继续实时发送。

受保护 Thinking output item具有明确 `ItemDone` 边界时，在经过 stream Hook 的完整 item上持久化 Marker并立即输出 comment；terminal projection复用已经交付的同一 Marker，不得重复创建。响应元数据、usage 和传输前缀不是内容边界，不提前封存仍可能收到原生签名的 Thinking；仅签名的工具载体与先前可读 Thinking 保持独立。Target stream没有提供完整 item边界时，保留 terminal projection回退，不能猜测 signature delta代表整个受保护单元结束，也不能为调整 comment位置缓冲后续可见输出。

如果 Marker 持久化失败且客户端输出尚未 commit，返回普通 typed error；已经 commit 时只能发送 ingress 协议的 terminal stream error。该策略不保证会丢弃 tool-call assistant `content` 的第三方客户端回传 Marker；缺失 Marker按客户端删除隐藏片段处理，不再通过上下文猜测。

## Target and storage boundaries

历史推理采用以继续会话为目标的 Thinking Replay，而不是将密文可表示性作为 Target 准入硬约束。平台为新产生的 Thinking 保存实际 Target、Provider 账号/配置 namespace、模型、协议来源和签发作用域。签发作用域只包含能影响签名/密文校验的事实：出口协议、部署（base URL）、凭据身份（OAuth 连接或凭据指纹），以及协议要求时的模型（目前仅 Gemini）；路由 Target、代理开关和 vendor 选项变化不使签名失效。

职责按来源与表示拆分。Core 只按来源决定受保护载荷（签名、`encrypted_content`、redacted block、Responses 原生 reasoning id）能否回放：签发作用域一致时保留，证明属于其它作用域时剥离；可读的摘要/正文始终保留为思考块。出口 codec（内置或插件）决定表示：原生推理载体能合法承载无签名明文时原生编码（如 Chat `reasoning_content`、Command Code `reasoning` part、Gemini thought part），只有原生载体会拒绝时才降级为普通 assistant 文本（Anthropic、Bedrock、缺少 id 与密文的 Responses reasoning）；承载不了的受保护载荷由 codec 忽略。这不是解密，也不声称摘要等价于完整推理。

权威历史和 History Marker payload 不因 Target 降级而改写。客户端保留原历史引用且记录未过期时，即使经过其他 Target、分支或 Gateway 重启，切回兼容来源仍可再次回放原密文。客户端提供的外部原生历史、来源记录已丢失的历史，或拆不出签发作用域的旧记录，不伪造来源，按来源不明乐观原生回放：这些载荷由客户端自己提交给它选择的路由，发给上游不构成额外披露，错误由上游校验暴露。上游在任何 canonical 输出前以 HTTP 400/422 或未携带 HTTP 状态的结构化流错误明确拒绝 encrypted content、thinking signature 或 Gemini thought signature 时，对当前 Target 分级做完整回放：先只剥离来源未证实的受保护载荷，没有可剥离的或再次被拒才剥离全部受保护载荷；普通错误、输出已开始或原生压缩操作不触发这一修正。发生推理降级后不使用旧 `previous_response_id` 代替改写过的完整历史。

以上只适用于历史推理。普通消息、公开及隐藏工具调用/结果、原生 compaction state 和其他硬约束继续严格校验。DeepSeek Chat 在携带 tools 时为缺失推理的 assistant 历史提供空 `reasoning_content`，不伪造占位推理，也不覆盖已有原生推理。

History Marker Store沿用现有 SQLite/PostgreSQL 存储安全边界，不单独增加应用层静态加密。`MemoryTurnChainStore` production和public路径直接删除：Gateway、Agent、Generation Chain、Web Search及测试统一使用 durable SQL Turn Chain；History Marker Store不提供内存事实源。

## Consequences

- 删除 `ToolContinuationStore` 及跨请求保存 live `InferenceRun` 的 claim/conflict/replay/successor路径；客户端再次提交工具结果时创建正常的新 Inference Run。Platform result去重和恢复正确性迁入 durable execution record。
- `HiddenRoundState` 继续作为单请求 P-only隐藏轮次的 usage/items临时累加；Generation Materialization Cache、Responses WebSocket Registry和Cache Affinity继续作为可丢失、可重建或纯性能的内存状态。
- Provider stream中公开 client call可能先于后出现的 Platform call完成 client commit；后续 Marker存储失败只能以 terminal stream error收尾。HTTP无法证明 Marker已到达远端客户端，已发布后断线可能留下执行过工具的 orphan记录，由保留策略清理但不能撤销外部副作用。
- 一个 model turn含多个 Platform calls或多个 protected Thinking blocks时生成多个 Marker；恢复器按客户端当前 Marker顺序收集这些一对一单元，再构造合法的 assistant calls和tool results顺序。
