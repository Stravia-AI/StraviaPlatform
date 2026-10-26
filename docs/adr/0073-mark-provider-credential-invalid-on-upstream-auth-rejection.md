---
status: accepted
---

# 上游凭据拒绝时持久化标记 Provider 凭据失效

当上游明确拒绝某 Provider 的当前凭据组合（401 / AuthenticationError），Stravia 将 `providers.credential_status` 持久化标记为 `invalid`：API key 凭据在第一次确认的上游认证失败即标记；OAuth 连接先在既有恢复预算内尝试刷新，仅在令牌端点明确拒绝授权（如 `invalid_grant`）、无可用 refresh token、本次操作的预算不允许启动恢复或刷新后的凭据仍被拒绝时标记。手动测试连接、模型发现、额度查询与独立能力操作适用同一认证证据规则。网络故障、超时、限流、上游 5xx、无法解析的成功令牌响应，以及没有明确授权拒绝证据的普通 400 / 403，不证明授权已撤销；即使刷新由原 access token 的 401 触发，也保留刷新故障本身的错误分类，不据此标记失效。403 权限失败、Hook 拒绝与客户端侧认证失败不构成凭据失效证据。

OAuth 凭据的 `status = error` 和 `last_error` 仅记录最近一次刷新失败，原凭据和连接身份保留，后续请求与后台调度仍可重试；它不等于 Provider 的 `credential_status = invalid`。失效 Provider 的全部 Target 在调度快照装配时失去资格，不参与新选择，但已开始的请求不中断。

失效只能由新凭据证据撤销：Provider 任何非展示字段（凭据、上游端点、协议、Vendor、代理等）变更、OAuth 重新绑定或刷新成功，以及手动连接检查明确验证凭据时复位为 `ok`；`is_enabled` 切换不复位。连接检查的 `credentials` / `endpoint_and_credentials` 成功构成凭据证据；`endpoint` 成功、静态模型、本地配置校验和不支持检查均不能清除失效状态。不做后台探测或半开重试——失效凭据的自动重试只会产生上游 401 噪音。标记写入以请求发出时的凭据版本（provider 行版本加 OAuth 连接版本）为条件，凭据已变化则放弃标记；已保存连接的检查结果还受插件 publication fence 约束，草稿检查不写测试状态或凭据健康。`credential_status` 对管理 API 只读。

不采用熔断器扩展（`TargetRuntimeState` 增加态）：失效是持久化 Provider 级凭据属性，熔断器是内存态、按 Target、自动恢复的瞬态语义，两者混用会重写整套 epoch 与探测语义。也不采用自动改写 `is_enabled`：那会混淆管理员意图与上游证据，凭据修复后将无法区分禁用来源。失效状态在管理面以红点呈现——Provider 启用中失效时主状态显示"凭据失效"，禁用中失效时以副行提示；Target 徽标新增 `credential_invalid` 态并压过冷却态。

共享刷新、并发等待、结果发布与升级边界见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)。
