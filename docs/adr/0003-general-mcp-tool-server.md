---
status: accepted
---

# Keep the MCP tool server independent of Web Access

Stravia 在 `stravia-core` 的公共 `mcp` module 中提供通用 MCP Tool interface、代码注册表和 Server 实现；Web Search 与 Web Fetch 只是注册到该 module 的首批 adapter，MCP 生命周期、认证和未来工具不得放进 Web Access seam。注册表按 Gateway 实例在启动时冻结，工具的运行时可见性由认证主体与原子配置快照决定；`McpTool` 与现有 `PlatformTool` 保持不同 interface，两者通过各自 adapter 复用领域 module。

## Considered options

- 把 MCP Server 放进 Web Access：一期文件更集中，但关闭 Web Access 会错误关闭未来非 Web 工具，注册接口也会继承无关的 Provider 语义。
- 合并 `McpTool` 与 `PlatformTool`：表面减少 trait，实际会把客户端可见 wire、隐藏模型轮次、执行上下文和错误协议堆进条件分支型浅 interface。
- 运行期增删注册项：支持插件热装卸，但需要共享可变 registry 与在途 schema 版本；一期只允许代码在 Gateway 启动时注册本地工具。

## Consequences

- Server 与 Desktop 都在 `POST /mcp` 暴露仅支持 MCP `2026-07-28` 的 Streamable HTTP Server；不提供 2025-era 入站兼容、stdio 或 MCP OAuth 2.1。
- MCP 使用现有模型 API key 作为自定义 `Authorization: Bearer` 凭据。全局 `web_access_enabled` 和该 API key 的 `mcp_access_enabled` 都必须为真；API key 权限默认关闭，且与透明注入权限相互独立。未配置相应 Web Provider 时，`tools/list` 不暴露该能力，猜测名称调用也失败。
- Web Access 开关、Provider 或优先级热更新会改变授权主体可见的工具列表，Server 因此声明并发送 `tools/list_changed`；MCP endpoint 本身保持存在，以便未来注册非 Web 工具。
- MCP 为每个工具同时返回模型可读 `content` 与遵循统一 output schema 的 `structuredContent`。协议错误使用 JSON-RPC error，执行错误使用 `isError`；批量 Fetch 全部失败时仍保留逐 URL 错误并设置 `isError:true`。
- 一期不增加 Stravia 自己的 MCP RPM、并发、金额预算或 OAuth 登录；上游限制与 Web Access 的 60 秒 deadline 仍然生效。
