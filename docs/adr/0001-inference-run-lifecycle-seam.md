---
status: accepted
---

# Place the Inference Run lifecycle behind one dispatcher seam

Stravia 将完整的 Inference Run 生命周期收进 `proxy/dispatcher/inference_run` crate-private deep module（`inference_run.rs` 的一次性执行 `interface` 与同名实现目录）：调用方只提交一次 `RunInput`，内部以私有状态机和所有权 guard 维护顺序不变量。这样既保留 `HookRuntime` 作为唯一推理扩展 `seam`、保留 Vendor 与客户端交付 `adapter` 的真实差异，又避免 dispatcher、流式 handler 与非流式 handler 分别重建 Hook、Platform Tool、Tool Continuation 和 Response Chain 编排。

## Considered options

- 纯命令式 orchestrator：外部 `interface` 最小，但内部顺序只靠代码纪律维持。
- 完整 typed-state `interface`：无效迁移更难表达，但会把状态复杂度、future boxing 和单态化成本带到 `seam`。
- Effect/Driver：事件与副作用最显式，但会扩大 `interface`，并给每个流式 delta 增加分派与潜在分配；当前也没有回放或持久化 run 的真实需求。
- Provider、Store、Delivery 三个私有 port：替换性最强，但 Store port 会把认证、Route/Target、Tool Continuation 与 Response Chain 聚成接近 implementation 的浅 `interface`。
- 采用的 owned-enum 混合方案：最小外部 `seam`，配合单写者 `Run`、私有 `Phase` 与 typed outcomes；Provider `adapter` 返回 canonical response/deltas，静态 `DeliveryAdapter` enum 交付 canonical IR，affine guard 维护 claim、completion、deadline 与 Hook leg。

## Consequences

- `InferenceRun` 不再是 stravia-core 的公开驱动 surface；正常调用方只能通过 dispatcher 内部 module 执行完整语义轮次。
- module 拥有 Request Hook、响应 Hook、串行 Platform Tool、隐藏轮次、Store 协调、Delivery 与全部 phase transition。一次 Model Turn（授权、Route/Target、failover、Provider Transport）交给 Model Turn Executor；Delivery `adapter` 接受 canonical IR，只负责 JSON/SSE encoding、backpressure、取消与 receiver-close observation。

- stream 与 non-stream 是同一私有静态 Delivery `adapter` enum 的两个真实 implementation；upstream stream 收集为 non-stream response 时仍使用 non-stream delivery，不形成第三套 lifecycle。streaming preflight 后由单一 producer task 独占 `Run`、Hook leg 与 leases，不使用共享 lifecycle mutex。
- Tool Continuation 和 Response Chain Store 保持独立的内部 deep module。新状态在语义完成时保存；被 claim 的旧 Tool Continuation 只在客户端交付完成后 complete，交付错误或中断时 release。
- 同一个 Inference Run 服从单写者约束。只有 Model Turn Executor 在第一次 canonical 输出前、且失败明确可重试时可以切换 Target；Hook、Platform Tool、状态不变量、取消和交付失败均终止 run。流式响应在首个合法 frame 已准备且 `Response` 交回 HTTP transport 时发生 Client Output Commit，正文成功结束才表示完整交付。

- JSON non-stream、upstream stream 收集和 client stream 必须共享 lifecycle，并保持 model、ID、tool correlation、reasoning、usage、stop reason 等 canonical invariants；不要求 wire、chunk 切分或最终结构逐字段全等，也不为复用 JSON parser 而缓冲正常 stream。
- 取消会立即停止未完成工作并完成幂等清理。流式路径只在现有 Hook 明确要求 terminal buffering 时缓冲；本次决定不引入隐藏轮次硬上限、持久化 Tool Continuation、新日志/wire 契约、通用 effect protocol、lifecycle observer 或每 delta dynamic dispatch。
- 迁移采用一次性干净切换。生命周期领域场景 contract matrix 通过同一 crate-private `interface` 覆盖三条 transport 路径；被替代的 handler 编排测试、`Response` extension 控制流、旧 helper 和重复 guard 一并删除，Provider/Delivery `adapter` 只保留 transport mechanics tests。
