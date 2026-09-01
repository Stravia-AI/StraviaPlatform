---
status: accepted
---

# 落盘 Allowance Sample 以支持预计耗尽

本决策修正 ADR 0029 中“不持久化快照”的部分。Monitor registry、凭据边界、官方额度端点、`use_proxy`、以及 Provider Allowance 不改变健康状态或路由资格，保持不变。

额度总览需要在重置前判断 Allowance Item 会不会耗尽。进程内快照重启即没，无法形成斜率；Stravia 的请求日志只覆盖本机流量，同一上游账户在其它客户端的消耗看不见，也不能把百分比窗口和 token 计数当成同一种量。

因此 Stravia 在每次成功的 fresh Monitor 读取时写入 Allowance Sample，并在 Gateway 进程运行期间每 30 分钟再采一次。Sample 按 Allowance Item（`provider_id` + 账户级 `allowance.key`）保存 used / remaining / reset，保留 14 天。当前页仍以 Monitor 快照为 live 事实；Sample 只服务于趋势和 Exhaustion Forecast。预报只使用当前重置窗口内的点做线性外推，跨重置的 Sample 丢弃；没有 `reset_at` 的余额只估计何时到 0。样本跨度不足 24 小时或有效点少于 2 个时不给出估计。Allowance Condition（正常 / 紧张 / 耗尽）由当前快照派生，不落盘，也不叫健康。

## Considered Options

把 live Provider Allowance 快照整份落盘，会让过期行变成第二事实源，和 Monitor 缓存抢权威。用 Stats Usage 当「近 7 日用量」实现便宜，但预报的是本机消耗，账户在别处烧额度时会显示没有风险。按日历近 7 日均值对齐了早期稿面，却会把 5 小时窗口和上周重置前的点接在一起。只在用户打开额度页时采样最省请求，桌面闲置后预报会长期空白。

## Consequences

ADR 0029 的 live 读取路径不变：列表和刷新仍走 Monitor，TTL 缓存仍只在进程内。新增的是派生观察记录，需要 SQLite 与 PostgreSQL 的 Sample 表、14 天清理，以及 Gateway 在跑时的后台采样；桌面在退出后停止采样。模型级额度不是 Allowance Item，不进入矩阵、时间轴或预报。管理面不得把 Allowance Condition 写成 Provider 健康，也不得用请求日志填预报。
