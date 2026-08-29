---
status: accepted
---

# Own Provider Transport behind the Model Turn Executor

Adding OpenAI Responses WebSocket creates a second real way to execute the same Model Turn. Stravia 决定把 Provider Transport 作为 Model Turn Executor 内部的 seam：HTTP/JSON 与 HTTP/SSE 是一个 adapter，Responses WebSocket 是另一个 adapter。Model Turn Executor 拥有 Route / Target 选择与第一次 canonical 输出之前的 failover（与 ADR-0007 对齐）；Inference Run 继续独占 Hook、Client Output Commit、Generation Chain 落盘与 Delivery。这样 transport 可以变化，而不会在 ingress、Vendor 或客户端交付路径复制一次推理状态机。


## Considered options

- 在 Open Responses WebSocket ingress 中直接连接 upstream WebSocket：入口实现最短，但 HTTP、SSE、其他客户端协议与 non-stream 请求会形成平行生命周期。
- 让 Vendor adapter 拥有完整调用与重试：能集中供应商差异，但会把连接、Target retry、deadline 与 Client Output Commit 拉过 Vendor seam。
- 让 Inference Run 独占 Target retry：HTTP 与 Agent Runner 会复制选路循环。拒绝。
- 为 WebSocket 建立独立 dispatcher：隔离 transport 最彻底，但会复制 Hook、Response Chain、Protocol Conversion 与 Delivery ordering。
- 在 Model Turn Executor 内建立 Provider Transport seam：两种真实 adapter 共享同一 canonical Model Turn interface，采用。


## Consequences

- HTTP 调用方仍只通过 Inference Run interface 提交 canonical RunInput；Agent Runner 通过同一 Model Turn Executor。不会出现 WebSocket 专用 Run 或第二种 Generation Chain 生命周期。

- Vendor adapter 继续拥有认证、URL、供应商 headers、request mutation 与 rolling OpenAI wire 差异；Provider Transport 只拥有连接、发送、接收、取消和 transport error。
- HTTP/SSE 与 Responses WebSocket 必须产出同一 canonical response/delta 与 typed error vocabulary，stream 和 non-stream 继续由既有 Delivery adapter 决定。
- Protocol Conversion 与 Representability 在 Provider call 前完成。WebSocket 覆盖率不能绕过等价性检查或扩张 dated Open Responses Protocol。
- 只有 Client Output Commit 前、且请求是否被接受没有歧义的 transport failure 可以在同一 Target 回退 HTTP/SSE；其后仍服从既有 Inference Run lifecycle。
- OpenAI direct、base URL override 与 Codex channel 通过 Vendor capability 选择 Responses WebSocket；其他 Vendor 不因 wire shape 相似而自动获得该能力。
- WebSocket client 复用现有 reqwest proxy、CONNECT、TLS 与认证配置，避免形成绕过管理员网络策略的第二条连接路径。
- Responses WebSocket 每连接只允许一个 in-flight response，并以 60 分钟 max-age 限制复用。当前不设置本地连接数上限，也不对有 affinity 的连接做 idle 回收；高并发 branch 的资源增长是已接受运维风险。
- `store=false` continuation 依赖同 socket tip。Affinity 丢失或 sibling 排队后 tip 已前移时，新连接发送完整 Effective Model Request，不把 connection-local upstream ID 带到新 socket。
- Transport 日志只记录 Target namespace、response/connection ID、连接年龄、fallback/replay 和 close reason；不记录 prompt、content、tool arguments、媒体或 credential。
