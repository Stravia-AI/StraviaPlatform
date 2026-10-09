# 通过本地 Monitor 读取 Provider Allowance

> “不改变路由资格”一项由 ADR 0078 部分修正：守护条目耗尽可经额度暂停影响路由资格。

Stravia 将 Provider Allowance 建模为只读的上游账户额度快照，由 Core 内的受信 Monitor registry 按已保存 Provider 的 Catalog 身份（`preset_key + channel`）选择实现。Monitor 只复用该 Provider 已保存的 Adapter Credentials 或 OAuth Credential，使用编译进 Core 的官方额度端点并遵循 `use_proxy`；它返回 typed allowance，不读取 `models.stravia.cn` 的可执行规则、不扫描其他应用的凭据、不持久化快照，也不改变 Provider 健康状态或路由资格。这样牺牲了远端动态扩展能力，换取凭据边界、请求目标、解析行为和版本兼容均由本地受审代码控制。

“不持久化快照”的领域边界由 [ADR 0032](0032-persist-allowance-samples-for-exhaustion-forecast.md) 补充为只持久化趋势 Sample，不把 live 快照变成第二事实源。当前快照缓存也不再限定为进程内 TTL：同一 Gateway 的统一派生缓存按 Memory / SQLite → TinyUFO、PostgreSQL → Redis 选择后端，成功快照 fresh 180 秒，last-good 最多保留 7 天且仍可提前淘汰。缓存失败或缺失不能冒充 fresh，官方 Monitor 仍是 live 事实源；合并并发读取与定时协调仍在本实例内。该修正不改变 registry、官方端点或凭据边界，也不提供跨实例或重启复用，见[存储架构](../design/architecture.md#统一派生缓存与-server-配置)。
