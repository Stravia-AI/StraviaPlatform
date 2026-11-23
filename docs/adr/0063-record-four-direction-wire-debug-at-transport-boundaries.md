---
status: accepted
---

# 在传输收发边界记录四方向 Wire Debug Capture

Wire Debug Capture 在实际应用协议边界捕获四方向：Connect Client→Stravia、Stravia→上游、上游→Stravia、Stravia→Connect Client。[ADR-0094](0094-own-vendor-egress-with-rama-and-patched-rustls.md) 将 Vendor 上游 HTTP egress 的请求与响应捕获放在 rama transport；请求头是按序列化规则生成的有序快照。WebSocket 在发送、接收和握手边界捕获，客户端在各自 ingress／egress 边界捕获。这不是 TLS、TCP、HTTP/2 frame 或 packet capture，也不保存 canonical、Hook 或投影中间态。

为保留诊断保真度，Wire 媒体不外置为 Artifact。普通 Observation 与 Debug 共享 Authorization-only 写入过滤：在入队、实时发布或存储前仅遮盖实际 HTTP `Authorization` header 值，名称不区分大小写。其他头、URL、媒体、正文、工具参数和授权可能保留，必须按敏感诊断处理。普通 Observation 仍只捕获原有语义范围，不变成完整 wire；日志、普通客户端错误、平台历史和模型侧凭据保护不放宽。

捕获失败记为 gap／partial，不影响推理。完整范围、存储和导出边界见[请求记录设计](../design/interaction-observation.md)。
