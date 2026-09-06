---
status: accepted
---

# 管理身份独立于推理身份，并支持立即撤销管理会话

本次用户系统设计只替换静态 Admin Token 管理认证：每个 Stravia 实例只有一个拥有全部管理权限的管理员，不提供多用户管理。模型调用与 MCP 继续使用现有 API Key 和 Principal，不增加 API Key 的管理用户归属关系，避免登录改造改变历史、配额及调用授权边界。

管理认证使用 JWT，但不采用仅验签的纯无状态授权。服务端必须同时校验可撤销的会话状态；退出登录撤销当前会话，修改密码撤销全部旧会话，撤销后的凭据不得通过后续认证。相比等待 JWT 自然过期，这增加了服务端状态校验，但能避免退出或修改密码后已泄露的管理凭据继续有效。

访问 JWT 有效期为 15 分钟，通过可撤销、轮换的 refresh token 将登录维持至最多 7 天。Server WebUI 使用 HttpOnly Cookie 与同源、CSRF 防护；Desktop 凭据仅保存在内存，通过 Bearer JWT 调用共享的管理认证实现。

管理凭据因此不能替代推理或 MCP 的 API Key，API Key 也不能登录管理面。Server 只通过 HttpOnly Cookie 承载登录凭据并执行精确 origin 与 CSRF 校验；Desktop 只允许受限原生通道取得内存中的 Bearer 凭据，普通回环 HTTP 调用不获得隐式信任。静态 Admin Token 已删除，不保留双认证路径。

完整运行契约见 [管理用户与首次初始化设计](../design/admin-auth-bootstrap.md)。
