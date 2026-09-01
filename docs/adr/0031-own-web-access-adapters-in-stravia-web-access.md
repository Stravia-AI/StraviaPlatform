---
status: accepted
---

# Own Web Access adapters in stravia-web-access

Stravia 将 Web Access 的全部 Web Provider 适配器从 `stravia-core` 迁到 `stravia-web-access`（由 `stravia-web-local` 改名）。crate 拥有 Local、Exa、Zhipu；删除 Brave 与 Tavily。公开 Web Search、Search Report、Local Agent 循环、Web Access 统一请求/结果与有序 failover 仍在 core；Codex Search Backend 本期不动。Local Web Provider 是恰好一条、不可删除的 `kind=local` 记录；出站跟随模型 Provider 的 `use_proxy` + Gateway `proxy_url`，不再把 Direct/System/Explicit 做成管理面。这样桌面与服务器共用同一套零 API key 的 Internal Search/Fetch，远程适配器不再寄生在 core 里，也避免为 Local 再开第二套配置根。

本决策接续 [ADR-0024](0024-local-web-provider-in-process-metasearch.md) 与 [ADR-0025](0025-in-process-web-fetch-quality-gate-chromium.md) 推迟的接线，并取代 [ADR-0026](0026-local-web-outbound-proxy-mode.md) 的管理面三档；search/fetch/Chrome 必须共用同一出站快照的约束仍然有效。

## Considered options

- 把公开 Web Search / Local Agent 塞进 crate：会让 Local Web Provider 变成 Search Backend，打穿已有 glossary。
- 只加 Local、保留全部远程 kind 并列：继续收 Brave/Tavily API key，和「本地替代远程」不符。
- 远程适配器留在 core、只接线 Local：同一 seam 两处实现，crate 名与所有权分裂。
- crate 命名为 `stravia-web-search`：与公开 Web Search 能力撞名。
- Local 做成不出现在 CRUD 的内置运行时：Web Provider 定义裂开，列表语义要另写一套。
- 管理面保留 Direct/System/Explicit，或让 Local 永远等于 `proxy_url`：前者与模型 Provider 的开关不一致；后者无法单独直连。
- 引擎开关放在 Web Access settings 或全局 cookie jar：Exa 在列表里时语义不清，且站点会话互相污染。
- 本期删除 Codex Search Backend：那是另一条 seam，会把 OAuth 固定模型与 Turn snapshot 绑进本次 adapter 搬家。

## Consequences

- Web Provider kind 仅为 `local` | `exa` | `zhipu`。每条记录都有 `use_proxy`（默认 `false`）；开启且 `proxy_url` 空则失败，关闭则直连。LocalWeb 运行时仍快照为 Direct 或 Explicit，System 不再出现在 Gateway 管理面。
- 迁移写入唯一 Local 记录。search/fetch 列表被删空（含去掉 Brave/Tavily 之后）则设为 `[local]`；已有 Exa/Zhipu 只剥已删 kind，不把 Local 插到队首。Brave/Tavily 记录删除，不提供读取兼容。
- Local 记录拥有 HTML Local Search Engine 的启用状态与引擎私有设置（可含按凭据保管的值）。默认启用 google/bing/brave/baidu；至少启用一个。计算器与 postsearch 不进该记录。本期不接小红书。
- 不用 Local 时从有序列表移除，不能删除该记录。全关引擎不是合法保存。
