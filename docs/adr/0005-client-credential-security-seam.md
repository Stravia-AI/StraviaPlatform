---
status: accepted
---

# Centralize client credential policy behind one security seam

Stravia 将 client credential 解析、Principal 认证、Model 访问授权、配额检查与 Model 列表可见性收进 crate-private `proxy/security` deep module。该 module 复用现有 `AuthAccessStore` seam，并以 typed outcomes / `GatewayError` 返回结果；Inference Run、models list 与 MCP adapter 不再各自实现 key state policy，也不把 `RequestContext` mutation、日志或 transport response rendering 放进 security implementation。

## Considered options

- 只删除未使用的公开 `check_model_access`，保留 Inference Run 私有认证 implementation：interface 最小，但 credential state、expiry 与错误 policy 仍散落在 Inference Run、models list 和 MCP caller，不能形成唯一事实源。
- 让 security module 通过 callback/session 拥有 Request Hook、post-Hook Model resolution 与 authorization ordering：能让顺序更难误用，但会把 ADR-0001 已交给 Inference Run 的 lifecycle implementation 拉过 seam，并形成接近 implementation 的泛型浅 interface。
- 新建 `ProxyAccessStore` 或 Gateway facade：会复制已有 `AuthAccessStore` 的真实 SQLite/PostgreSQL adapter seam，并把无共同不变量的 active Provider lookup 混入 access interface。
- 在 Inference Run 开始时冻结 access snapshot：减少数据库读取，但被禁用的 key、变更的 Model binding 或耗尽的 quota 仍可推进隐藏轮次，弱化现有 fail-closed 语义。

## Consequences

- security interface 使用拥有 secret 的 opaque credential value，并保留两种 transport profile：Inference 接受 Bearer、`x-api-key` 与 `x-goog-api-key`，MCP 只接受 Bearer。它按真实 caller 提供 required Principal、final Model authorization 与 visible Model IDs 等命名操作，不公开通用 policy mode 或动态 rule registry。
- Principal 在 Request Hook 前建立；Hook 可以改写 Model；Model binding 与 quota 只针对 post-Hook final Model，并在每个隐藏轮次重新读取 key state、binding 与 persisted usage。本 run 尚未持久化的隐藏轮次 token usage 会计入下一轮 TPM/TPD 判断；Target retry 在同一轮复用授权结果。Active Provider 与 Provider Model lookup 留在 Inference Run implementation。
- 所有 Inference Model 都要求有效 API Key，且最终 Model 必须存在于该 Key 的绑定列表；不再存在 public Model 或 Anonymous Principal 降级。缺失或无效凭据返回 HTTP 401，disabled Key 返回 HTTP 403，expired Key 统一采用 canonical `GatewayError::Unauthorized(AuthFailure::Expired)`（HTTP 401）。models list 同样 fail-closed，仅返回有效 Key 已绑定的 Model；MCP 保持 401/403/503 transport mapping。
- `proxy/security` 收紧为 crate-private；重复的 `ProxyAccessStore`、Gateway adapter、Provider helper、`inference_run/auth.rs` 与 caller-local key-state policy 在迁移时一次性删除，不留 alias 或兼容路径。`AuthAccessStore` 保持唯一 storage seam；没有 runtime 需求时不为该 refactor 新增 Memory adapter。
- access policy tests 只通过 security interface 覆盖 credential state、Model binding、RPM/RPD/TPM/TPD 与 typed errors；Inference Run 只保留 Principal-before-Hook、post-Hook final Model authorization 和 hidden-round re-evaluation 的 ordering matrix。
