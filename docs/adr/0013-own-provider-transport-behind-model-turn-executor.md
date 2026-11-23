---
status: accepted
---

# Own Provider Transport behind the Model Turn Executor

## 背景与决策

HTTP/JSON、HTTP/SSE 和 Responses WebSocket 是执行同一次 Model Turn 的不同传输，不是独立的推理生命周期。Provider Transport 留在 `ModelTurnExecutor` 下，统一输出 canonical 响应或增量及 typed errors。Executor 拥有 Route/Target 选择和首个 canonical 输出前的 failover；Inference Run 保留 Hook、Client Output Commit、Generation Chain 交付后落盘和 Delivery。Vendor 负责上游认证、URL、headers 与 wire 差异，连接和传输错误不由客户端入口处理。

Vendor 的 HTTP 与 WebSocket 共享宿主受控出站，由 rama 与补丁版 rustls 承担（见 [ADR-0094](0094-own-vendor-egress-with-rama-and-patched-rustls.md)）。是否启用 WebSocket 由 Vendor 能力选择，不靠猜测 wire 形态。只有尚未对客户端提交输出、且可确认请求未被接受时才允许同 Target 回退 HTTP/SSE。`store=false` 的 connection-local tip 不能跨 socket；失去 affinity 时重发完整 Effective Model Request。

## 权衡

在客户端 WebSocket handler 内直接连接上游最短，却会复制 Hook、选路、历史和交付状态机；让 Vendor 接管重试又会把 failover 与 deadline 分散到各插件。共用 Model Turn 边界保留传输差异，同时避免第二套生命周期。当前续接与连接约定见 [Open Responses](../design/open-responses.md) 和 [Vendor 插件](../design/vendor-plugins.md#wit-契约与受控出站网络)。
