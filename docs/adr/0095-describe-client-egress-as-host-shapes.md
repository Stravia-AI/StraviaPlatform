---
status: accepted
---

# 用 Egress Shape 描述客户端出站形态

Egress Shape（出站形态）是可按名称选择的连接层形态，包括 TLS ClientHello、ALPN、HTTP/1 协议字段位置、HTTP/2 参数及 WebSocket 握手方式。它不是供应商 Profile，也不是 User-Agent、凭据或重试配置。1.0 采用宿主内置数据，插件通过 `shape` 选项引用；未知名称失败，不回退到其他客户端身份。

请求字段由插件按顺序构造，宿主不补 `Accept`、`Accept-Encoding` 或 `User-Agent`。协议必需的槽位字段由宿主校验并在指定位置填值；HTTP/2 的协议限制仍须执行。连接池与 TLS 会话不得跨出站形态及其修订复用，代理和目标身份继续参与复用约束。

基准来自仅连接本地监听器、使用占位凭据的真实客户端采集；记录客户端版本和平台，不把模拟客户端的数据冒充官方样本。Codex HTTP 初始对齐范围为 Linux OpenSSL；TCP/IP 和时序指纹不在范围内。内置数据的代价是更新形态需要发布宿主，但无需更改 WIT。

本决定细化 [ADR-0071](0071-orchestrate-vendors-through-host-network-capabilities.md) 的字段所有权，宿主继续拥有授权、重试、deadline 和 failover。
