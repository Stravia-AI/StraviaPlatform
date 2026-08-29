---
status: accepted
---

# Rename Web Research to Web Search and split public and internal tool identities

Stravia 将原 `Web Research` 领域全量 clean cutover 为 `Web Search`：文档、源码类型与模块、前端/REST contract、settings key、错误码、日志和 turn identity 使用新名称，旧公共路径、旧配置 key、旧别名不再保留。公开 composite 仍使用 wire name `web_search` 并返回原有带来源的 Search Report、continuation 和 branch 语义；Local Agent 内部继续使用 wire names `web_search` 与 `web_fetch`，但源码使用不同的 Tool ID、注册面和 owner，使 public composite 与 internal leaves 即使 wire name 相同也永不共用身份。

Codex Agentic Search 仍固定管理员选择的 OAuth Provider/账号和上游模型，但不使用 Stravia 的 Local research step/time budgets；Codex 模式隐藏 Local Web Provider 与 Local limits，切回 Local 时恢复已保存配置。请求取消、请求生命周期和传输安全边界仍有效。既有 `web_research_config` 的配置值迁移到新 settings key；旧 Research Turn 历史失效，不提供旧 identity 的读取兼容。

## Considered options

- **只改用户文案**：会让 Web Search 产品名与 WebResearch 源码/REST/持久化身份长期分裂。
- **让 public composite 与 internal leaves 共用 Tool ID**：会重新引入 AgentTool registry、MCP discovery 和 tool ownership 冲突。
- **给 internal leaves 改用带前缀的 wire name**：源码更直观，但破坏 Local Agent 既有 `web_search`/`web_fetch` 工具契约。
- **保留旧 Turn 的兼容读取**：可以保留历史续接，但会在 clean cutover 后维护双 identity；本次选择让旧历史失效。
- **让 Codex 继续接受 Local 研究预算**：会对 hosted Agentic Search 重复施加 Stravia-owned loop 限制；Codex 应由其上游执行边界负责内部研究。

## Consequences

- REST 和持久化身份迁移是 breaking change；旧客户端路径与旧配置 key 不再读取，历史 Research Turn 不可续接。配置值迁移但旧历史不迁移。
- Search Report provenance、Search Turn snapshot、MCP/PlatformTool surface 和 Local hidden leaves 的行为不因改名改变。
- 公开 `web_search` 与内部 `web_search` wire name 相同，代码审查必须检查 distinct IDs/registries，禁止以名称推断 owner。
- Codex 研究页不再显示 Local Provider、研究限制或 Codex 数据出境提示；移除提示降低了用户对数据发送范围的可见性，这是本次接受的产品风险。
