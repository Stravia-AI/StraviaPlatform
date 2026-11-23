---
status: accepted
---

# 把 live 与 staged Client Projection 收进同一 session

Live 与 staged 曾分别处理受保护的 Thinking、工具分类与 History Marker，并靠两套账本对账。Stravia 将两条输入统一到 Client Projection，由它以相同规则将 canonical delta 投影给客户端。Protocol Conversion 提供协议载体形状，不参与投影；Inference Run 仍拥有 Hook、Delivery、模型循环与工具执行。

投影协调 Marker 的预留与交付确认；调用方只报告发送或取消，不能在发送前自行发布。保留独立的 live gate 会使两条路径继续漂移；让投影自己发送则会把背压和取消所有权从 Delivery 拉入投影。执行边界见[整体架构](../design/architecture.md)，历史载体契约见 [Open Responses 设计](../design/open-responses.md)。
