---
status: accepted
---

# 在传输收发边界记录四方向 Wire Debug Capture

## 决定

Wire Debug Capture 保持四个方向：Connect Client→Stravia、Stravia→上游、上游→Stravia、Stravia→Connect Client。记录发生在各自传输的收发边界：上游 HTTP 在 reqwest 请求/响应边界捕获；上游 WebSocket 在对应 WebSocket transport 的发送、接收与握手边界捕获；客户端 ingress/egress 在对应传输边界捕获。Debug 不整体迁移为 reqwest 专属路径。

捕获保留应用层实际收到或交付的内容及其顺序，按应用协议表达，不宣称 TLS、TCP、HTTP/2 frame 或其他网络分包的原始保真。reqwest 请求/响应捕获不是 Network Capture 或 Packet Capture。

## 隐私边界与后果

仅对 `Authorization` header 的值永久脱敏，header 名大小写不敏感。媒体保留捕获内容，不因媒体类型替换为 Artifact 引用；其他 header、URL 与 body 不额外脱敏。因此 Cookie、`x-api-key` 及 body 仍可能包含凭据，Debug Trace 与导出包必须按可能含凭据的敏感诊断处理。

本 ADR 只在 Debug Wire 范围内取代 ADR-0060 中“凭据在落盘前统一脱敏”的约束、ADR-0048 §「上传凭据的历史与持久化边界」中“诊断记录不得保存真实 Artifact Upload Grant”的条款，以及 ADR-0049 §「媒体内容的持久化边界」中 Wire 媒体外置的条款。ADR-0060 未单独规定媒体外置；媒体原样保留同时取代此前 `CONTEXT.md` 的 Wire Debug Capture 定义及 `docs/design/interaction-observation.md` §6.2 中关于媒体外置、不可恢复与 omission 的约束。上述例外不改变普通 Observation、历史、日志、模型上下文或业务侧凭据保护与媒体外置规则。

## Debug 内容范围

Debug 仅保存四方向原始应用协议收发的 wire 载荷，以及 Run/Interaction/attempt 关联、方向、传输与协议、顺序与时间、状态、编码和捕获完整性所需元数据。它不保存 canonical request/response、Hook 前后内容、Client Projection 内容，也不保存独立的 canonical 阶段或其他内部诊断阶段记录。

原始应用协议捕获独立于普通 Observation 的按 item 收口内容，不将媒体转换为 Artifact 引用。捕获失败沿用 `partial` / `gap` 表达，不能影响推理。普通 Interaction Observation 的既有生命周期、用量和工具事件不受影响。仅 Debug 使用本 ADR 的 Authorization header 脱敏与媒体原样边界，普通 Observation 既有脱敏策略不变。

这是已接受并已迁移的 Wire Debug Capture 契约；普通 Observation 的 Canonical Item 持久化由 ADR-0062 独立约束。
