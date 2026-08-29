# Web Search 设计

> 状态：已实施
> 更新：2026-08-17
> 相关决策：[ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md)、[ADR-0017](../adr/0017-rename-web-research-to-web-search-and-split-tool-identities.md)

## 1. 结论

Web Search 是一个由平台总开关控制的 Advanced Capability。它向普通模型请求和 MCP 提供同一个公开 composite `web_search`，返回带来源的 `SearchReport`；Local 与 Codex backend 的差异不进入公开 contract。

平台总开关决定能力是否存在。开关开启后，每个有效 API Key 都可以显式调用；关闭后，普通请求和 MCP 都不可用。API Key 的 Transparent Injection 只决定 Stravia 是否在客户端未声明工具时自动暴露 `web_search`，不承担显式调用授权。

## 2. 公开 contract

公开 wire name 保持 `web_search`。输入字段：

```json
{
  "query": "question or topic",
  "previous_turn_id": "wst_...",
  "allowed_domains": ["example.com"],
  "blocked_domains": ["blocked.example"]
}
```

`query` 必填。`previous_turn_id` 用于继续或分支既有 Search Turn；domain filters 可选。

成功结果：

```json
{
  "turn_id": "wst_...",
  "completion": "complete",
  "report": {
    "answer": "Verified answer [source-wst_...-1]",
    "sources": [
      {
        "id": "source-wst_...-1",
        "url": "https://example.com/source",
        "title": "Source title"
      }
    ],
    "limitations": []
  }
}
```

`SearchReportValidator` 保证：

- answer、sources、limitations 满足大小和数量边界；
- 每个 source ID 由当前完整 `SearchTurnId` 限定；
- answer 中的 marker 与 sources 一一对应；
- URL 是规范化后的公网 HTTP(S) URL；
- source 必须来自当前或祖先 Turn 的已验证 evidence；
- partial 结果必须说明预算或超时限制。

`SearchTurn` 是 principal-scoped、不可变的 continuation point。根 Turn 固定 backend、binding、配置 revision 和 Local budget snapshot；子 Turn 可继续或从任一可访问父节点分支。当前持久化 identity 为 `kind = "web_search"`，ID 前缀为 `wst_`。

## 3. 能力门控与透明注入

一次公开调用必须同时满足：

1. `web_search_config.enabled = true`；
2. API Key 存在、启用且未过期；
3. `WebSearchRunner` 已配置。

调用面规则：

| 调用面 | 额外条件 | Transparent Injection 的作用 |
|---|---|---|
| 客户端显式 `web_search` | 平台 Gate 开启 | 无 |
| Hosted/native web search 声明 | 平台 Gate 开启 | 无 |
| Stravia 自动暴露 `web_search` | Key 的 master 与 `inject_web_search` 均开启 | 决定是否暴露 |
| MCP `tools/list` / `tools/call` | `mcp_access_enabled` 与平台 Gate 均开启 | 无 |

关闭平台 Gate 后，Key 上已保存的 `inject_web_search` 不删除；运行时忽略它。重新开启 Gate 后，该选择恢复生效。

## 4. Tool identity 与 surface

公开 composite 和 Local leaves 可共享 wire name，但不能共享源码身份或 registry owner：

| Owner | Source Tool ID | Wire name | Hook/Platform | MCP | AgentToolRegistry |
|---|---|---|---:|---:|---:|
| Public composite | `web-search` | `web_search` | 是 | 是 | 否 |
| Internal search leaf | `web-access.search` | `web_search` | 否 | 否 | 是 |
| Internal fetch leaf | `web-access.fetch` | `web_fetch` | 否 | 否 | 是 |

`GatewayBuilder` 先用调用方传入的 Platform/MCP tools 和两个 internal Web Access leaves 构建 `AgentToolRegistry`，再把 public Web Search composite 加入 Hook 和 MCP registry。public composite 因而不会形成 Agent → Agent nesting；internal leaves 也不会进入普通客户端或 MCP discovery。

Local Definition 使用 `id = "web-search-local"`、`slug = "web_search_local"`，并且 `exposure = Internal`。

## 5. Backend

### 5.1 Local

`LocalSearchBackend` 通过 internal Agent Definition 执行 search/fetch loop：

- 管理员选择一个已启用、支持 tool calls 的逻辑 Model；
- Web Access 设置提供 Local search/fetch Provider 与优先级；
- `max_turns` 和 `total_time_seconds` 作为 `LocalSearchLimits` 传入；
- deadline 取调用方 request deadline 与 Local total time 的较早值；
- `SearchReportValidator` 用本次运行收集的 evidence 校验最终报告。

### 5.2 Codex

`CodexAgenticSearchBackend` 固定管理员选择的 OAuth Provider/账号和 upstream Model。它使用 Codex hosted web search，不读取或执行 Stravia 的 Local turns/time budget。

Codex 仍受以下边界约束：

- 调用方 cancellation；
- 外层 request deadline；
- Provider eligibility、OAuth credential 和固定 upstream Model；
- 传输、响应大小、SSE 完成状态和引用 annotation 校验。

配置仍保存 Local budget 数值。Codex 模式忽略这些值；切回 Local 后重新显示并校验原值。

## 6. Admin 与持久化

Admin REST canonical paths：

- `GET` / `PUT /api/v1/web-search/config`
- `GET /api/v1/web-search/eligible-models`
- `GET /api/v1/web-search/codex-providers`

settings canonical key：`web_search_config`。配置包含 `revision`、`enabled`、backend binding、`max_turns`、`total_time_seconds` 和 `updated_at`。

SQLite 与 PostgreSQL migration `0018_advanced_capabilities_web_search.sql`：

- 把旧 settings 值移到 `web_search_config`；
- 把 Turn kind 约束切换为 `web_search`；
- 删除旧 Research Turn，因为 clean cutover 不允许跨 identity continuation；
- 同时迁移 API Key 的 Transparent Injection 字段。

## 7. 安全与日志

- URL normalization 和 DNS 检查拒绝 localhost、私网、非 HTTP(S) 和解析到非公网地址的 source；
- Local 网页内容是不可信数据，不得作为指令执行；
- progress event 只包含 call ID、phase 和 ordinal；不包含 query、URL、报告、usage 或凭据；
- audit identity 使用 `web_search` / `web_search_codex_request` / `search_turn_id`；
- 对外错误使用 `WEB_SEARCH_*` 或 `web_search_*` identity，不返回 Provider raw body、OAuth token、API key、headers 或堆栈。

## 8. Breaking upgrade 与回滚

这是 clean cutover，不提供 alias：

- 旧 `/web-research/*` REST path 不再存在；
- 旧 `web_research_config` settings key 只由 migration 读取一次；
- 旧 `kind = "web_research"` Turn 被删除，不能续接；
- 旧 `allow_web_research` 和 `web_search_injection_enabled` API Key 字段被删除；
- 旧二进制不能安全读取迁移后的 schema。

升级前必须同时备份数据库和当前二进制。回滚必须停止新二进制，并恢复 migration 18 之前的数据库备份与匹配的旧二进制；不能只回退应用文件，也不能把新 schema 手工解释为旧权限模型。

## 9. 验证边界

- Admin API：配置读写、Local/Codex validation、旧字段拒绝；
- Gateway public contract：Gate、有效 Key、显式调用、Transparent Injection 与 MCP 组合；
- Search contract：Search Report provenance、continuation、branch 和 `wst_` identity；
- registry：三个 source Tool ID 不同，public composite 不进入 Agent registry，internal leaves 不进入 MCP；
- migration：SQLite/PostgreSQL schema parity、settings 值迁移和旧 Turn 失效；
- WebUI：Advanced Features 导航、独立页面、Codex 条件隐藏和 Local 值恢复。
