---
status: accepted
---

# 落盘 Allowance Sample 以支持预计耗尽

## 背景与决策

Provider Allowance 由使用上游官方额度端点的 Monitor 读取；当前额度以 Monitor 快照为准。成功的新读取另写 `Allowance Sample`，供趋势与当前重置窗口的耗尽预测使用。样本不是实时值或健康状态；守护条目可以影响路由资格，但暂停依据是实时额度，不是历史采样（[ADR-0078](0078-suspend-provider-routing-on-guarded-allowance-exhaustion.md)）。

## 权衡

本机 `Stats Usage` 不含该账号在其他客户端的消耗，不能预测共享额度；可丢弃的实时快照不承诺跨重启留存，整份持久化则容易形成第二事实源。独立持久化样本允许预测跨重启连续；按当前窗口计算，避免把不同重置周期和计量类型拼成一条斜率。

Monitor、额度与选路见 [系统架构](../design/architecture.md)；快照缓存边界见 [统一派生缓存与 Server 配置](../design/data-layout-performance.md#统一派生缓存与-server-配置)。
