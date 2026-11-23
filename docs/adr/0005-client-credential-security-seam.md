---
status: accepted
---

# Centralize client credential policy behind one security seam

## 背景与决策

crate-private `proxy/security` 是模型 API Key 解析、Principal 验证、Model 授权、额度检查与模型可见性的共同入口，复用 `AuthAccessStore`。Inference Run、models list 与 MCP 不复制 API Key 状态规则；Hook 顺序、请求上下文变更、日志与 transport 错误渲染仍由各自调用方负责。

在 Request Hook 前取得 Principal，随后对 Hook 选出的 Model 执行授权。新隐藏轮次重新检查当前 API Key 状态和适用权限；同轮 Target retry 不是新的根请求。模型列表只对有效 API Key 返回已绑定模型，不向匿名身份降级。

## 权衡

冻结一次授权快照能减少读取，但失效的 API Key 或消耗完的额度还会推进后续隐藏轮次。让 security 回调 Hook 又会夺走 Inference Run 的生命周期所有权。独立规则与明确调用顺序可以同时保留 fail-closed 和各传输真实差异。当前准入及用量边界见 [系统架构](../design/architecture.md) 和 [费用与计费](../design/cost-and-billing.md)。
