---
status: accepted
---

# 守护条目耗尽时持久化暂停 Provider 路由资格

## 背景与决策

管理员可为支持 Monitor 的 Provider 显式守护账户级 Allowance Item。成功的新读取发现任一守护条目耗尽时，持久化暂停该 Provider 的路由资格；只有新的成功读取确认不再耗尽，或管理员取消相关守护，才解除。缺项与读取失败不改变旧状态；`reset_at` 只触发重新读取。读取凭据版本与证据顺序约束同 [ADR-0073：凭据失效](0073-mark-provider-credential-invalid-on-upstream-auth-rejection.md)，避免旧结果覆盖当前状态。

暂停与管理员启用状态、Target 冷却、凭据失效和 [Allowance Sample](0032-persist-allowance-samples-for-exhaustion-forecast.md) 独立。调度不使用已暂停 Provider 的 Target，但不打断在途请求，也不改写路由配置。

## 权衡

仅在进程内暂停会在重启前后改变路由资格；自动改写 Target `enabled` 会覆盖管理员意图；按重置时钟或单次 `QuotaExceeded` 推断全账户状态又缺少可靠证据。持久化窄状态并等待 Monitor 证明，接受读取间隔内的检测延迟。当前暂停、恢复与错误契约见 [系统架构](../design/architecture.md)。
