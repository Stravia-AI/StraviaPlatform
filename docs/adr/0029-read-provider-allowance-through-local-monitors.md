# 通过本地 Monitor 读取 Provider Allowance

Stravia 将 Provider Allowance 建模为只读的上游账户额度快照，由 Core 内的受信 Monitor registry 按已保存 Provider 的 Catalog 身份（`preset_key + channel`）选择实现。Monitor 只复用该 Provider 已保存的 Adapter Credentials 或 OAuth Credential，使用编译进 Core 的官方额度端点并遵循 `use_proxy`；它返回 typed allowance，不读取 `models.stravia.cn` 的可执行规则、不扫描其他应用的凭据、不持久化快照，也不改变 Provider 健康状态或路由资格。这样牺牲了远端动态扩展能力，换取凭据边界、请求目标、解析行为和版本兼容均由本地受审代码控制。
