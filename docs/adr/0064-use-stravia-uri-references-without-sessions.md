---
status: accepted
---

# 统一 Stravia 引用外壳但不引入 Session

平台用 `stravia://artifacts/<artifact-id>` 表达 Artifact Reference，用 `stravia://turns/<turn-id>` 表达 Turn Reference。两者是稳定、不透明的地址，不是访问凭据；解析仍按当前 Principal 校验，Artifact 字节传输另用 HTTP(S) 授权。不同 Turn 种类仍受各自约束。选择 `turns` 而不是 `sessions`，是为了指向不可变的精确续接节点：从可访问旧节点续接产生独立分支，没有隐式 latest、可变 head 或独立 Session。Open Responses 的 `id`、`previous_response_id` 与内部 `TurnNodeId` 不变。

平台结果统一用 `path` 表达自身引用，不重复返回裸 ID。`stravia://turns/<turn-id>/sources/<ordinal>` 是 Search Turn 下的证据来源地址，不是独立资源或可续接节点。报告证据引用写完整 URI，URI 本身不证明来源或归属。媒体报告的本次输入与实际证据继续分开：后者可以引用历史 Artifact；续接身份由可信工具历史恢复，不依赖正文提示。

旧平台引用直接切换，不提供别名或批量改写历史。StraviaRead 以 Turn URI 作 `path`，追问或新增媒体放在 `body`；Artifact 读取用末尾 `:` 选择器。其他平台工具仍可按各自契约用 `previous_path` 指定父 Turn。这区分了资源地址与操作参数，不引入第二套平台续接身份。

[Agent Core 设计](../design/agent-core.md)拥有平台引用、Turn Chain、Artifact 与 StraviaRead 通用契约。
