---
status: accepted
---

# 把 live 与 staged Client Projection 收进同一 session

ADR-0033 要求 streaming 与 non-streaming 物化同一 Client Projection，并把 reserve → 落盘 → 交付 → publish 交给 Client Projection。前一次加深已经把 staged 收进 `ClientProjectionSession`；live 路径仍把 protected 缓冲、tool 名分类、Marker 账本和 Sent→publish 写在 Inference Run 的 stream producer 与 `LiveDeltaGate` 里。两套 implementation 靠 HashSet 对账，改 Post-Text 或 Marker 顺序必须同时改 session、gate 和 producer。

Stravia 删除 `LiveDeltaGate` 作为平行 module。session 吃 canonical delta、吐 projected delta；live 与 staged 是同一对象上的两条输入。Inference Run 仍拥有 Hook 变换、Delivery 发送、Model Leg 循环、Platform Tool Execution 与 terminal Hook 缓冲。caller 回报 Sent / Cancelled，session 只在 Sent 后 publish。Thinking 载体形状由 Protocol Conversion 查询，Client Projection 不 `matches!(egress)`。

## Considered options

- session 吃 `CanonicalEvent`，连 Hook 与 Delivery 也藏进去：producer 消失，但 interface 膨胀到接近 Inference Run，违反 ADR-0001。
- 保留 `LiveDeltaGate` 为平行 module，或作为带 trait 的内部 indexer：改动小，live 与 staged 仍是两套 implementation；一条 adapter 构不成真 seam。
- session 内部 `matches!(egress)` 决定 protected 缓冲：最快，但 Client Projection 开始认识上游协议，和「不是 Protocol Conversion」打架。
- Model Turn Executor 或 stream producer 继续按协议 id 算载体形状：知识仍漏在投影之外，下次改 Open Responses 摘要还要进 `stream/mod.rs`。
- session 自己拿 Delivery adapter 发送：顺序不容易写错，但把 backpressure 与取消拉进 Client Projection。
- 落盘后立刻 publish、不等 Sent：断开时 Marker 已可恢复但客户端没收到，和 ADR-0033 的交付后失败必须终止冲突。
- Platform Marker 留在 completion/producer：Thinking 与 Platform 两套账本，`projected_marker_carriers` 去不掉。

## Consequences

- 领域词不新增。对象仍是 Client Projection、History Marker、Reserved Thinking Marker、Protocol Conversion、Inference Run。
- ADR-0033 的产品契约不变。该 ADR 末句把 tool 名分类、protected Thinking 积累和 Marker 顺序屏障留在 stream 路径；本决策把它们收进 Client Projection 的 implementation。协议组帧、UTF-8 完整性、terminal Hook 缓冲和 Delivery 仍留在原处。
- `.scratch/deepen-client-projection/spec.md` 里「LiveDeltaGate 留在 Inference Run」不再有效。
- Protocol Conversion 向 Client Projection 提供 Thinking 载体形状（是否 indexed、是否可能 protected、未保护 summary 能否直播），不执行投影。
- 主测试打 session interface。Inference Run `execute()` 只保留 disconnect、Marker 失败不提交 Generation Chain、Client Output Commit。`LiveDeltaGate` 单测删除。
