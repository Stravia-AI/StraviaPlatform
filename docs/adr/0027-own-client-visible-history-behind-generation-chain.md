---
status: accepted
---

# Own client-visible history projection and node legality behind Generation Chain

Generation Chain 节点保存的是 ingress 协议下的客户端可见投影，不是 Effective Model Request 的 items。Stravia 把该投影和节点合法性收进 Generation Chain：`begin` / `stage` / `discover` 拥有 Gemini tool-id 改写、Open Responses item reference、Chat/Anthropic 展平等协议形态的 IR 改写；`Write.stage` 判定一份响应能否成为节点，并在此时写入投影。Inference Run 只在完整交付后 `commit()`。Protocol Conversion 保持 pair-bound 的 decode / encode / stream，不再通过 `ProtocolAdapter` 暴露历史方法。

## Considered options

- 让 Protocol Conversion 增加投影阶段：规则跟 codec 在一起，但 ProtocolPair 超出 ADR-0006 的 decode/encode/stream，ingress / Model Turn / Delivery 会看见用不到的历史 interface。
- Generation Chain 内部再做 projector trait：四个 ingress 确实有差异，但三个 ProtocolAdapter 方法并不是同一套操作（Gemini 改 prefix，Responses 要 catalog，Chat 只展平）。新 trait 会再做出一条浅 seam。
- 只搬投影、不动 Write 落盘：ContextBag、terminal 判断、Open Responses profile 仍漏在 completion/stream/delivery。

## Consequences

- 领域词不新增。对象仍是 Generation Chain、Generation Chain Write、Effective Model Request、Protocol Conversion。
- ProtocolAdapter 只保留 decode / encode / stream。删除 `project_client_history`、`resolve_item_references`、`item_reference_node_ids`。ingress 身份仍可由 `inferred_ingress` 识别，那不是历史投影。
- `Write.stage` 只接受 `completed` / `incomplete`（无终态字段时等价于无 error）；`failed`、取消与 delivery failure 不入 staged。投影与 persist profile 在 stage 时写入。Inference Run 只在完整交付后 `persist()`。
- Inference Run 持有 `GenerationChainWrite`，不再经 RequestContext bag 传递；删除 `PendingGenerationChainPersist`。
- 测试打 Generation Chain 的 `begin` / `stage` / `persist` / `discover`，以及 `execute()` 的交付时机。codec 投影单测删除。
