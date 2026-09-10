# Web Search 设计

> 状态：已实施
> 更新：2026-09-10
> 相关决策：[ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md)、[ADR-0017](../adr/0017-rename-web-research-to-web-search-and-split-tool-identities.md)

## 1. 结论

实现归属独立 `stravia-web-search` crate，包含 Runner、Local/Codex Backend、Definition、报告与证据校验、公开工具、透明注入及配置策略。`stravia-core` 在编译期注入 Agent、Provider 快照、设置与授权的 Host Adapter，并保留 MCP/管理面 Adapter；能力不反向依赖 core。共享执行类型来自 `stravia-runtime-contract`，域名规范化及内部工具 ID 归 `stravia-web-access-contract`，静态地址规则继续归 `stravia-web-access`。

Web Search 是一个由平台总开关控制的 Advanced Capability。普通模型请求与 MCP 通过 `StraviaRead` 的 `query://` 输入执行完整研究并返回带来源的 `SearchReport`；Local 与 Codex backend 的差异不进入公开 contract。网页读取共用该开关和透明注入选择。参见 [ADR-0051](../adr/0051-disambiguate-artifact-download-and-understanding.md)。

平台总开关决定联网能力是否存在。开关开启后，每个有效 API Key 都可以显式调用；关闭后，普通请求和 MCP 的联网分流均不可用。API Key 的 Transparent Injection 只决定是否自动暴露 `StraviaRead` 的联网分流，不承担显式调用授权；执行层强制检查本次暴露范围。

## 2. 公开 contract

公开 wire name 为 `StraviaRead`，不保留旧平台工具调用别名。输入字段：

```json
{
  "url": "query://question%20or%20topic",
  "previous_turn_id": "wst_...",
  "allowed_domains": ["example.com"],
  "blocked_domains": ["blocked.example"]
}
```

`url` 必填，搜索文本采用 URL 参数编码。`previous_turn_id` 用于继续或分支既有 Search Turn；domain filters 只用于 `query://`。外部网页 URL 的原始查询参数不解释为工具参数。

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
| 客户端显式 `StraviaRead` 联网分流 | 平台 Gate 开启 | 无 |
| Hosted/native web search 声明 | 平台 Gate 开启 | 无 |
| Stravia 自动暴露联网分流 | Key 的 master 与 `inject_web_search` 均开启 | 同时暴露搜索和网页读取 |
| MCP 联网调用 | `mcp_access_enabled` 与平台 Gate 均开启 | 无；裸 Artifact 下载独立可用 |

关闭平台 Gate 后，Key 上已保存的 `inject_web_search` 不删除；运行时忽略它。重新开启 Gate 后，该选择恢复生效。

Web Access 不另设总开关。`WebAccessSettings` 只包含 `search_provider_ids` 与 `fetch_provider_ids`，分别表示搜索与网页读取来源的有序选择；管理 HTTP 与 Desktop 使用同一结构。旧实例的 `web_access_enabled` 设置不再读写，也不再决定运行时可用性。该调整不自动启用 `web_search_config.enabled`，不改变 API Key、MCP 或网络安全校验。

管理界面仅保留顶部 Web Search 启停开关。Local 模式开启前必须有有效的已保存模型绑定，以及可用的搜索与网页读取来源；缺少配置时就地提供补齐入口。来源选择与排序独立即时保存，模型绑定和预算仍显式保存；启停不提交表单草稿，Codex 模式不依赖 Local 来源。

## 4. Tool identity 与 surface

统一入口与内部 Local 检索使用相同 wire name，但 registry 由受信执行环境分别装配，模型不能选择执行层级：

| Owner | Source Tool ID | Wire name | Hook/Platform | MCP | AgentToolRegistry |
|---|---|---|---:|---:|---:|
| Public router | `stravia-read` | `StraviaRead` | 是 | 是 | 否 |
| Internal read router | `stravia-read` | `StraviaRead` | 否 | 否 | 是 |

`GatewayBuilder` 先为内部 Agent 装配基础读取 router，再在外层 Hook/MCP registry 装配公开 router。外层 `query://` 调用现有完整 Web Search owner；内部 `query://` 只调用基础检索。两者的网页 URL 均复用现有 Web Access owner。插件通过 `PlatformTool::read_domain()` 贡献可判定领域、非空简短描述与处理入口；重复领域或冲突身份显式失败，不通过描述或加载顺序路由。

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

### Web Access 静态规则与执行检查

`stravia-web-access::address_policy` 是 Web Access IP 分类与 HTTP(S) URL 静态规则的唯一所有者。`is_public_ip(IpAddr)` 分类一个地址；`allows_url(&Url)` 接受已解析 URL，只返回静态允许或拒绝，不分配、不重新解析 URL、不执行 DNS。域名静态通过不代表实际目的地址安全。

core 保留输入修整、错误映射、准入与异步解析调度；adapter 保留解析错误映射、每次重定向、已取得的全部 DNS 地址检查、连接地址固定和代理 DNS 分工。fetch 经代理时仍不新增本地 origin DNS；其他原有入口的解析检查也不删除。浏览器导航、子资源和最终 URL 检查继续执行；`about:`、`blob:`、`data:` 的处理仍由 browser adapter 路径拥有，不进入共享 HTTP(S) 规则。

迁移前发现 `http://127.0.0.1../`、`http://192.168.1.1../` 的实际差异：core 去除尾随点后按 IP 拒绝，adapter 原先按域名静态接受。经维护者明确授权，共享规则采用 core 的既有拒绝语义；这是 adapter 静态接受范围的一项收紧，不是完全行为等价的重构，也不构成已证明的 SSRF 绕过结论。core 的 Unicode `trim()` 与 adapter 原始输入解析差异保留。

维护时先运行共享分类和现有 fetch/browser 执行测试，再运行受影响 crate 检查与仓库工作流。分类测试不能替代混合/空/失败 DNS、私网重定向、代理不解析 origin、socket 出口与真实浏览器子资源的行为验证。有限输入的迁移前后比较不是所有地址和 URL 的形式化等价证明。

### Search Source 与日志

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
