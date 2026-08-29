---
status: accepted
---

# Own agent execution behind AgentRunner and TurnChain seams

Stravia 在 `stravia-core` 内原生拥有 agent execution：从现有 Inference Run 下方提取 transport-neutral `ModelTurnExecutor`，由 `AgentRunner` 在其上执行有界 model→Platform Tool→model loop，并把成功结果提交为可分支的 `Agent Turn`。历史复用只通过 principal-scoped `AgentTurnId` 与共享 `TurnChain` 表达，不引入 `AgentSession`、外部 agent runtime 或第二套工具 seam；这样模型调用、canonical IR、Hook、安全、quota、provider routing 与 Desktop/Server 部署继续只有一个 owner。

完整 interface、状态机、持久化、Artifact、多模态与 adapter 设计见 [`docs/design/agent-core.md`](../design/agent-core.md)。

> ADR-0009 部分取代本决策的 Admin 与 capability authorization：generic Agent list/patch Admin API 将 clean cutover 删除；Media/Web Research 等产品 capability 由专用 flag 授权其 hidden Model，不再要求 key 的 `model_ids` 包含该 Model。普通 proxy 与 generic `agent_<slug>` 的 model binding 不变。

## Considered options

- 直接依赖 Rig：Rust 原生且已有 `AgentRunner`、typed hooks 与 structured output，但它自带 provider client，会与 Stravia 争夺模型调用所有权；其 MCP SDK major 也与项目当前版本不共享类型。
- 直接依赖 Swiftide：Rust 原生且适合 RAG/task graph，但同样引入第二 provider/tool integration layer，且不提供 Stravia 所需的持久化会话 backend。
- 外置 LangGraph、OpenAI Agents SDK、Google ADK 或 Microsoft Agent Framework：可复用更成熟的 checkpoint/workflow 能力，但需要 Python、JavaScript、Go 或 .NET sidecar，破坏 Desktop 单 Rust 进程部署，并增加 IPC、凭据、生命周期与观测面。
- 递归调用 `dispatch_pipeline`：可快速复用现有调用链，但每个 agent model step 都会嵌套完整 Inference Run，重复 auth、Hook、delivery 状态，并产生 agent capability 自递归风险。
- 直接把 Inference Run 扩展成 AgentRunner：少一个新模块，但会把客户端交付、provider turn、工具循环与历史 DAG 合并成一个浅而庞大的 interface。
- 让 AgentRunner 直接依赖 `PlatformTool`：接口最少，但无法统一版本化的入站/远程 MCP tools。最终采用 crate-internal `AgentTool` adapter seam；它只规范化 ID@version、schema、执行上下文与结果，不拥有第二套 transport 或 provider client。
- 使用 provider conversation ID 或可变 `Session` head：存储更轻，但会绑定 provider，并引入 provider state 与本地 canonical history 两个事实源；并发分支的 implicit latest 也无法稳定定义。

## Consequences

- `ModelTurnExecutor` 只负责一次 canonical model turn：动态 Principal/Model/quota 检查、Route/Target、provider codec、重试、streaming 与 usage；它不执行工具、不拥有历史或客户端交付。
- Inference Run 与 AgentRunner 分别在 `ModelTurnExecutor` 上编排。AgentRunner 不调用 HTTP ingress、`dispatch_pipeline` 或 Vendor/reqwest shortcut。
- AgentRunner 只使用代码内 `AgentDefinition Revision` 固定的 `AgentTool ID@version`；PlatformTool 与获准的 MCP tool 通过 adapter 注册，客户端 function tool 不穿透。内层 Hook 的 `ExposeTool` 必须拒绝，agent-as-tool nesting 禁止。
- 每次成功或受预算约束完成只提交一个 immutable Agent Turn。调用无 parent ID 时创建根；给定旧 AgentTurnId 时恢复精确父链；同一父节点可产生多个不合并分支，不存在独立 Session ID 或 implicit latest。
- Response Chain 与 Agent Turn 共用 `TurnChain` deep module。Response Chain 迁入 SQLite/PostgreSQL，默认 retention 与 Agent Turn 一致为可配置 7 天；Tool Continuation 不随本 ADR 改为 durable。
- Agent Definition 由程序注册，管理员只能配置全局 enabled 与逻辑 Model ID。新根固定当前 Revision/Model；旧 Turn 固定原 Revision/Model。Definition 停止注册后不再创建新根，但未过期 Turn 仍可执行，因此相关 Revision 与 ToolVersion 必须保留到引用过期。
- AgentRunner 保持 request-bound、poll-driven event stream；drop/父请求取消即取消 Run。后台 durable task、MCP Tasks 与 provider background execution 只能作为未来 adapter 加入，不能改变 AgentRunner interface。
- 图片、视频和研究不进入 AgentRunner 状态机。canonical IR 增加 `Video`；opaque `ArtifactId` 由 AgentInput/ArtifactStore seam 解析并 materialize 为 canonical media。能力不支持时 typed reject；抽帧、转录和 WebAccess research 必须以后通过显式版本化 AgentTool 接入。
- 本决策不提供调用幂等、显式 Turn/Artifact 删除或 context compaction。它们的生产风险与验收要求记录在设计文档中，不能由实现静默补成另一套生命周期或状态模型。
