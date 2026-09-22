---
status: accepted
---

# 按 Canonical Item 收口持久化诊断内容

Interaction Observation 的 canonical 诊断内容按 Canonical Item 边界持久化：同一 item 的流式碎片汇聚为一项并保留项内 part 边界，独立 item 即使类型相同也不合并。该决定取代 `docs/design/interaction-observation.md` §5.1 中诊断文本按时间或大小封块的设计约定；实现尚未迁移。

正常结束时保存完整 item；可处理的失败或取消发生时，保存已实际收到的内容并将该 item 标为未完成。系统不周期性持久化同一 item 的中间快照，因此进程突然崩溃可以丢失整个尚未落盘的 item；本决策不规定如何补齐取消时未实际收到的尾部。

实时观测和下游转发不等待诊断写入，Generation Chain 仍只提交完整交付且终态合法的节点。本决策不包含 Debug 原始传输字节捕获：它不是 canonical item 聚合记录，其四方向原始收发契约由 [ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md) 决定。
