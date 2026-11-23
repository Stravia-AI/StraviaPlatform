---
status: accepted
---

# Keep the MCP tool server independent of Web Access

## 背景与决策

Desktop 与 Server 的 `POST /mcp` 是通用 Streamable HTTP 入口，不归 Web Access 所有。Gateway 启动时固定代码注册表；鉴权主体和运行时配置决定工具的可见性与调用授权，而不是增删注册项。MCP 使用模型 API Key 的 Bearer 凭据并要求 `mcp_access_enabled`；平台能力仍各自检查 Gate，透明注入选择不决定 MCP 可用性。

`McpTool` 与 `PlatformTool` 分开适配，同一领域能力可以复用执行逻辑，但客户端 wire、发现和错误规则不会进入隐藏模型轮次。

## 权衡

把 MCP 放进 Web Access 会让非 Web 工具继承无关的 Provider 生命周期；统一 `McpTool` 和 `PlatformTool` 则会把协议、执行上下文和权限堆进条件分支。启动时注册避免为动态装卸维护在途 schema 版本，同时允许配置变化更新可见工具列表。当前工具和门禁见 [Agent Core](../design/agent-core.md#平台能力门禁与透明注入)。
