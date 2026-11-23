---
status: accepted
---

# Place the Inference Run lifecycle behind one dispatcher seam

## 背景与决策

dispatcher 通过 crate-private `inference_run` 提交一次 `RunInput`，由单写者 Run 统一编排 Hook、串行 Platform Tool 及隐藏轮次、Tool Continuation、Generation Chain 和交付。一次 Model Turn 的授权、选路与 failover 交给 `ModelTurnExecutor`；`HookRuntime` 保持唯一推理扩展接口。流式与非流式交付只在编码、背压和断线感知上不同，不拥有推理编排。

## 权衡

纯命令式编排成本小，却只能靠调用方遵守顺序；公开完整类型状态或 Effect/Driver 又会把内部状态与逐增量分派成本暴露给调用方。因此将 Phase、typed outcomes 和清理 guard 留在模块内部，不引入可被多个调用方驱动的状态机。

## 后果

首个 canonical 输出前的可重试上游错误由 Model Turn Executor 处理，取消、交付故障与 Hook 故障不走 Target failover。将响应交回 HTTP transport 不等于完整交付；历史提交和旧 Continuation 的完成判定不能把两者混为一谈。完整请求链与 Hook 顺序见 [系统架构](../design/architecture.md)。
