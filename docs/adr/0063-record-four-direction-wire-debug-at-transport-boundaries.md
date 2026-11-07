---
status: accepted
---

# 在传输收发边界记录四方向 Wire Debug Capture

修订：Vendor 1.0 的上游 HTTP 捕获随 [ADR-0094](0094-own-vendor-egress-with-rama-and-patched-rustls.md) 移到 rama 传输，取代下文上游 reqwest 边界的实现描述。请求头是依照序列化规则生成的有序应用层快照，不是底层字节抓包；应用层捕获范围、隐私和权限规则不变。

## 决定

Wire Debug Capture 保持四个方向：Connect Client→Stravia、Stravia→上游、上游→Stravia、Stravia→Connect Client。记录发生在各自传输的收发边界：上游 HTTP 在 reqwest 请求/响应边界捕获；上游 WebSocket 在对应 WebSocket transport 的发送、接收与握手边界捕获；客户端 ingress/egress 在对应传输边界捕获。Debug 不整体迁移为 reqwest 专属路径。

捕获保留应用层实际收到或交付的内容及其顺序，按应用协议表达，不宣称 TLS、TCP、HTTP/2 frame 或其他网络分包的原始保真。reqwest 请求/响应捕获不是 Network Capture 或 Packet Capture。

## 隐私边界与后果

仅对 `Authorization` header 的值永久脱敏，header 名大小写不敏感。媒体保留捕获内容，不因媒体类型替换为 Artifact 引用；其他 header、URL 与 body 不额外脱敏。因此 Cookie、`x-api-key` 及 body 仍可能包含凭据，Debug Trace 与导出包必须按可能含凭据的敏感诊断处理。

本 ADR 的 Wire 媒体原样保留取代 ADR-0049 §「媒体内容的持久化边界」中 Wire 媒体外置的条款，以及此前 `CONTEXT.md` 和所链接设计中的对应约束；普通 Observation 的媒体捕获范围和外置规则不变。普通 Observation 现也采用相同的 Authorization-only 写入过滤，因而 ADR-0060 的凭据统一脱敏承诺及 ADR-0048 的诊断记录不得保存真实上传授权承诺，对普通 Observation 与 Debug Wire 均不再适用。历史、进程日志、面向客户端的普通错误、模型上下文和业务侧凭据保护不放宽。

## Debug 内容范围

Debug 仅保存四方向原始应用协议收发的 wire 载荷，以及 Run/Interaction/attempt 关联、方向、传输与协议、顺序与时间、状态、编码和捕获完整性所需元数据。它不保存 canonical request/response、Hook 前后内容、Client Projection 内容，也不保存独立的 canonical 阶段或其他内部诊断阶段记录。

原始应用协议捕获独立于普通 Observation 的按 item 收口内容，不将媒体转换为 Artifact 引用。捕获失败沿用 `partial` / `gap` 表达，不能影响推理。普通 Interaction Observation 的既有生命周期、用量和工具事件范围不受影响；两者只对实际 HTTP `Authorization` header 值遮盖，名称不区分大小写，body 或工具参数中同名字段不属于 header。普通记录的实时展示、落盘和导出也可能保留下载授权及其他凭据，不要求开启 Debug。普通 Observation 不因此变为完整 wire 捕获；保留期、权限不变，旧记录不回写，下载授权不自动刷新或续签。

这是已接受并已迁移的 Wire Debug Capture 契约；普通 Observation 的 Canonical Item 持久化由 ADR-0062 独立约束。
