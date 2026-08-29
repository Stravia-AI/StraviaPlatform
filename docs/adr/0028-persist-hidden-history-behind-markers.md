---
status: accepted
---

# Persist hidden history behind one-to-one markers

Stravia 将客户端可见历史与 Provider 有效历史保持为两个视图：客户端投影只用 Principal-scoped History Marker 表示未披露内容，History Marker Store 持久化实际 Hidden History Segment，恢复请求时在 Marker 原位置替换后再发送上游。Marker 不依赖周边上下文匹配；客户端可以修改其他历史。一个 Marker 只对应一个 Platform Tool Execution 的 call/result 对，或一个受保护 Thinking block，禁止聚合多个工具执行或多个 block。

## Client projection

- 所有 ingress 协议都把 Platform Tool call/result 隐藏为 Marker。协议无法无损回传 opaque thinking 时，可见 reasoning 继续输出，一个独立 Thinking Marker保存该 block 的 opaque 部分；原生可无损表示时继续使用原生字段。
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
- 后台 execution继承 R1 的 Principal Concurrency Limit名额直到 terminal。匹配 Marker 的后续请求可以先认证并等待；execution terminal释放名额后，后续请求再正常竞争执行名额。
- 只有 Platform Tool、没有 client tool 时，在同一客户端 stream输出 Marker，等待 execution完成后继续下一 Model Turn；不强制客户端创建额外请求。

## Streaming

实时 streaming 是底线，不允许为了 Marker 缓冲整个 Model Turn。普通可见 delta 立即发送；每个尚未分类的 tool index只缓冲到名称可分类，Platform call继续按该 index缓冲到 `ToolCallComplete`。完整 call到达后一次事务持久化 Marker/execution，输出对应 Marker，隐藏该 Platform call的全部 wire delta并开始执行；公开 text、reasoning 和 client-tool delta继续实时发送。

受保护 Thinking output item具有明确 `ItemDone` 边界时，在经过 stream Hook 的完整 item上持久化 Marker并立即输出 comment；terminal projection复用已经交付的同一 Marker，不得重复创建。Target stream没有提供完整 item边界时，保留 terminal projection回退，不能猜测 signature delta代表整个受保护单元结束，也不能为调整 comment位置缓冲后续可见输出。

如果 Marker 持久化失败且客户端输出尚未 commit，返回普通 typed error；已经 commit 时只能发送 ingress 协议的 terminal stream error。该策略不保证会丢弃 tool-call assistant `content` 的第三方客户端回传 Marker；缺失 Marker按客户端删除隐藏片段处理，不再通过上下文猜测。

## Target and storage boundaries

恢复 opaque thinking 后，Target只按 egress协议能否无损表示进行筛选；OpenAI-compatible Chat Target应跳过，没有可表示 Target时返回 typed `protected_context_unrepresentable`。不绑定原 Provider、credential namespace或 model，也不把 opaque内容降级成明文或静默丢弃。

History Marker Store沿用现有 SQLite/PostgreSQL 存储安全边界，不单独增加应用层静态加密。`MemoryTurnChainStore` production和public路径直接删除：Gateway、Agent、Generation Chain、Web Search及测试统一使用 durable SQL Turn Chain；History Marker Store不提供内存事实源。

## Consequences

- 删除 `ToolContinuationStore` 及跨请求保存 live `InferenceRun` 的 claim/conflict/replay/successor路径；客户端再次提交工具结果时创建正常的新 Inference Run。Platform result去重和恢复正确性迁入 durable execution record。
- `HiddenRoundState` 继续作为单请求 P-only隐藏轮次的 usage/items临时累加；Generation Materialization Cache、Responses WebSocket Registry和Cache Affinity继续作为可丢失、可重建或纯性能的内存状态。
- Provider stream中公开 client call可能先于后出现的 Platform call完成 client commit；后续 Marker存储失败只能以 terminal stream error收尾。HTTP无法证明 Marker已到达远端客户端，已发布后断线可能留下执行过工具的 orphan记录，由保留策略清理但不能撤销外部副作用。
- 一个 model turn含多个 Platform calls或多个 protected Thinking blocks时生成多个 Marker；恢复器按客户端当前 Marker顺序收集这些一对一单元，再构造合法的 assistant calls和tool results顺序。
