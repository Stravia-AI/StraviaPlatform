---
status: accepted
---

# Own agent execution behind AgentRunner and TurnChain seams

## 背景与决策

`AgentRunner` 原生拥有有界 model→tool→model 循环，与普通推理共用 transport-neutral `ModelTurnExecutor`，不递归调用 Gateway。`AgentDefinition` 以代码注册，Revision 固定可用的 `AgentTool` 版本；产品访问与管理由各能力 adapter 负责。客户端 function tool 和公开 Agent composite 不进入内层工具表，因此不会递归嵌套 Agent。

历史使用 Principal-scoped `AgentTurnId` 与共享 `TurnChain`。一次完成的 Run 形成一个不可变 Turn；给定父 Turn 才精确续接，可从同一父节点分支，没有可变 Session 或隐式 latest。续接保留原 Definition Revision 和逻辑 Model，不因管理员更新默认值而重解释历史。

## 权衡

外部 Agent 框架已有工作流与 checkpoint，但会引入第二套供应商调用、工具或部署运行时。把 Run 循环直接塞进 Inference Run 又会耦合客户端交付与 Agent 历史。原生 runner 保留一个模型执行边界；用例的媒体预处理、搜索或 Artifact 读取留在 adapter，不进入循环状态机。

Definition、Revision、Runner 与历史契约见 [Agent Core](../design/agent-core.md)。
