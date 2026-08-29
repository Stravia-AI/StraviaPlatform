---
status: accepted
---

# Put Web Search and Web Fetch behind one Web Access seam

Stravia 将 Web Search 与 Web Fetch 收进独立的 Web Access deep module；其小 interface 只接受统一请求并返回统一结果，Codex OAuth、Exa API、Brave API、Tavily API 与智谱 Coding Plan Remote MCP 作为 Web Provider adapter 隐藏在 module 内。Platform Tool、通用 MCP Server 与 OpenAI Responses adapter 分别处理自己的可见性和 wire 语义，但都调用同一个 Web Access interface；不创建同时承担模型循环、MCP 协议和供应商细节的通用 Tool abstraction。

## Considered options

- 每个供应商暴露独立工具：能保留全部供应商参数，但会让模型和客户端耦合部署拓扑、凭据与故障切换。
- 在 Web Access 内嵌通用 Agent Execution Core：能统一生成回答，但会给索引型搜索强加隐藏模型调用；一期复用现有 Inference Run 工具轮次，未来需要时以不同语义新增 `web_research`。
- Search 与 Fetch 共用一个 Provider 顺序：配置较少，但 Brave 与 Codex 不支持确定性 Fetch；采用独立的有序优先级列表。

## Consequences

- `web_search` 接受不超过 2,000 字符的 `query`、默认 5 且范围 1–20 的 `max_results`，以及各不超过 20 个的 `allowed_domains`/`blocked_domains`。域名按小写 IDNA hostname 匹配自身和子域，block 优先，冲突输入无效；Brave 用 query rewrite 尽力检索并严格 post-filter 输出，rewrite 超过 Brave 限制时跳过该 adapter。
- Search 输出为 `{mode, query, results, answer?, citations?}`；`mode` 区分索引型与 agentic Web Provider。结果不公开 Web Provider ID、原生 request ID、usage、成本或尝试链，也不伪造不可比较的 score。
- `web_fetch` 接受 1–20 个公网 HTTP(S) URL，默认每 URL 8,000 字符、可配置范围 1,000–50,000 字符，并以 64,000 字符总上限公平收紧批量结果。输出按输入顺序保留每个 URL 的成功、截断或稳定错误；部分失败只把失败 URL 交给下一个 Fetch Provider。
- Search 与 Fetch 各按管理员顺序故障切换；同一 adapter 不重试，空 Search 结果是成功，整次调用共享 60 秒 deadline。配置热更新只影响新调用，在途调用使用启动时不可变快照；Stravia 不缓存结果。
- 一期 Web Provider 为：引用现有 Codex Provider OAuth 凭据的 agentic Search adapter、Exa API Search/Fetch、Brave API Search、Tavily API Search/Fetch。匿名 Exa、OpenAI API-key Web Provider、Provider 高级参数、统一内容审核、图片搜索和本地速率/预算/每 Run 上限不在一期范围内。
- 后续加入的智谱 Coding Plan adapter 以同一 API key 分别连接其 Search 与 Reader Remote MCP，仍只向上层暴露统一 `web_search`/`web_fetch` 契约；上游 MCP 的会话、工具名和双重 JSON 编码结果不穿透 Web Access seam。
