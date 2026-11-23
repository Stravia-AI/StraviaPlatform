---
status: accepted
---

# 按 Canonical Item 收口持久化诊断内容

Interaction Observation 按 Canonical Item 聚合和持久化诊断内容，而非定时或定长切块；同一 item 内保留 part 边界，不仅因为类型相同就合并独立 item。这使持久记录对应模型或客户端的语义单位，代价是进程突然崩溃时可能丢失尚未完成落盘的整个 item。

正常结束后保存完整 item；可处理的失败或取消保存实际收到的片段并标记未完成。实时观测与下游转发不等待该写入，Generation Chain 的提交条件不变。具体事件和持久化由[请求记录设计](../design/interaction-observation.md)拥有；[ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md) 的四方向 Wire Debug 不是 item 聚合。
