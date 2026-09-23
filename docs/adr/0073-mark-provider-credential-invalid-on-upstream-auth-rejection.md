---
status: accepted
---

# 上游凭据拒绝时持久化标记 Provider 凭据失效

当上游明确拒绝某 Provider 的当前凭据组合（401 / AuthenticationError），Stravia 将 `providers.credential_status` 持久化标记为 `invalid`：API key 凭据在第一次确认的上游认证失败即标记；OAuth 连接在平台刷新恢复手段耗尽（刷新失败、无 refresh token、刷新后仍被拒绝）后标记。失效 Provider 的全部 Target 在调度快照装配时失去资格，不参与新选择，但已开始的请求不中断。手动测试连接、模型发现与用量查询中的上游认证失败适用同一判定；403 权限失败、Hook 拒绝与客户端侧认证失败不构成证据。

失效只能由新凭据证据撤销：Provider 任何非展示字段（凭据、上游端点、协议、Vendor、代理等）变更、OAuth 重新绑定或刷新成功、手动测试成功均复位为 `ok`；`is_enabled` 切换不复位。不做后台探测或半开重试——失效凭据的自动重试只会产生上游 401 噪音。标记写入以请求发出时的凭据版本（provider 行版本加 OAuth 连接版本）为条件，凭据已变化则放弃标记；`credential_status` 对管理 API 只读。

不采用熔断器扩展（`TargetRuntimeState` 增加态）：失效是持久化 Provider 级凭据属性，熔断器是内存态、按 Target、自动恢复的瞬态语义，两者混用会重写整套 epoch 与探测语义。也不采用自动改写 `is_enabled`：那会混淆管理员意图与上游证据，凭据修复后将无法区分禁用来源。失效状态在管理面以红点呈现——Provider 启用中失效时主状态显示"凭据失效"，禁用中失效时以副行提示；Target 徽标新增 `credential_invalid` 态并压过冷却态。

本 ADR 记录已确认的目标语义，尚不表示实现已完成。
