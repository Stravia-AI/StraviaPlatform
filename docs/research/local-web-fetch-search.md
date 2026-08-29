# Stravia 本地 `web_fetch` 与 `web_search` 技术选型研究

> 调研截止与来源访问日期：2026-08-17。本文只依据官方仓库、官方文档、RFC、crate 源码或 docs.rs；“活跃”仅表示来源中可观察到的发布日期、CI 或仓库状态，不代表 SLA 或质量承诺。
> **范围校正：** 本文选择的是 `WebResearchRunner` 所消费的 `web_search` 与 `web_fetch` 两个 leaf tool/provider 实现；不替换 runner、研究循环、模型编排或 UI。所谓“本地实现”允许本机服务访问公网 URL/搜索引擎；只有明确要求断网或自有 corpus 时，才需要本地 crawler+index。

## 结论

1. **近期 `web_fetch` 应先做 in-process、HTTP-first 的 native fetch。** 以现有 `reqwest::Client` 为传输层，以 `url::Url` 做严格解析，以 RFC 9309 规则和 Rust `robotstxt` parser 做 `robots.txt` 判定，以 `scraper`/`html5ever` 解析静态 HTML，再以 `dom_smoothie` 做正文抽取。这覆盖“给定 1–20 个公网 URL，返回受限正文”的真实 leaf 语义，不引入 crawler、队列、索引或浏览器进程。`reqwest` 已在仓库中使用；其官方文档提供超时、代理、重定向策略、TLS、压缩和自定义 DNS/地址能力（[reqwest README](https://github.com/seanmonstar/reqwest)、[`ClientBuilder`](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)、[redirect policy](https://docs.rs/reqwest/latest/reqwest/redirect/index.html)）。
2. **静态 HTML 与动态页面必须是两个有界阶段。** `scraper` 是浏览器级 HTML5 DOM/CSS 查询库，但不会执行 JavaScript（[官方 docs.rs](https://docs.rs/scraper/latest/scraper/)）。`dom_smoothie` 复刻 `readability.js`，明确承认正文结果仍可能不理想（[官方 crate README](https://docs.rs/crate/dom_smoothie/latest)）。若 JS-heavy 支持是明确需求，仍应先走 native HTTP fast path，正文为空、只得到 SPA shell 或命中显式 render policy 时再调用 Firecrawl/Chromium；浏览器必须有独立并发、内存、时间和网络预算。
3. **`Spider` 适合后续 crawler/站点采集，不适合近期单页 leaf。** 它是 MIT 的 Rust crawler，HTTP-first、按需启用 Chrome，支持 link discovery、robots、depth、concurrency、输出 Markdown/JSON/WARC 及分布式扩展（[官方 README](https://github.com/spider-rs/spider)、[docs.rs crate metadata](https://docs.rs/crate/spider/latest)）。这些能力对 Stravia 当前明确 URL 的 `web_fetch` 是额外重量；先采用 `reqwest` 组合，未来站点 crawl PoC 再单独评估。
4. **本任务中的“local search”首先指本机运行的 search tool，不强制自有索引。** 若目标是替代 `web_search` leaf，SearXNG、Websurfx 或 open-webSearch 即使查询公网 upstream，也已满足“本地进程、本地配置、统一结果”的工具替代要求。只有额外要求断网查询或 Stravia 自持 corpus 时，才进入 YaCy 或 Spider+Tantivy；`Tantivy` 单独只是索引 building block，不是可替代 `web_search` 的完整服务（[Tantivy README](https://github.com/quickwit-oss/tantivy)）。
5. **公网 `web_search` 的生产默认是自托管 SearXNG；Websurfx 仅作 Rust-only/实验对照。** 两者都是本地运行的元搜索，不是自有索引；SearXNG 的 API/配置和维护信号更适合默认接入，Websurfx 需先锁定 rolling JSON 契约与 provenance。SearXNG 官方将自身定义为聚合多个 search services/databases 的 metasearch，并在 API 文档中明确 query 会传给 external search services；官方容器部署依赖 Docker/Podman，查询结果依赖配置的上游 engines（[SearXNG README](https://github.com/searxng/searxng)、[Search API](https://docs.searxng.org/dev/search_api.html)、[configured engines](https://docs.searxng.org/user/configured_engines.html)、[container install](https://docs.searxng.org/admin/installation-docker.html)）。
6. **完全本地 crawler+index 的可选 PoC 是 YaCy（跨语言、完整应用）或 Spider+Tantivy（Rust 组合）；不推荐把已归档的 Stract 作为新生产依赖。** YaCy 官方说明它包含 index server、web UI、production-ready crawler/scheduler，并可退出 P2P、只查 local index；官方还给出 Java 17+、Windows installer 和 Docker 路径（[YaCy README](https://github.com/yacy/yacy_search_server)、[仓库元数据](https://api.github.com/repos/yacy/yacy_search_server)）。Stract 的 README 证明其 own crawler/independent index，但官方 API 截至本日标为 `archived: true`、最后 push 为 `2025-03-24T08:42:45Z`，所以只适合作为 Rust 搜索架构参考或受控实验（[Stract README](https://github.com/StractOrg/stract)、[Stract API metadata](https://api.github.com/repos/StractOrg/stract)）。
7. **最小运行时集成仍可保持 `WebProviderAdapter`，但持久化配置不是“只加一个 adapter”。** native fetch 能复用 runtime trait；现有 Rust model 虽把 `api_key` 表示为 `Option<String>`，SQLite/PostgreSQL 最终 schema 都将其设为 `NOT NULL`，且 kind constraint 只允许四种远程 provider，admin 也要求 API key。因此 native kind 同样需要 schema/admin 变更。SearXNG/YaCy 还需新增受校验的 `base_url`/options，并同步 Admin API 和 WebUI。运行时 adapter 仍只实现现有 `WebProviderAdapter::fetch/search`；`WebAccessEngine` 继续负责 deadline、字符上限、fallback。native adapter 还必须每次 redirect 重新做 SSRF 检查，并让连接使用经过检查的地址，防 DNS rebinding/TOCTOU。
8. **三档轻量路线与单体结论：** 只做 `web_fetch` 时，最轻 Rust 集成是 native `reqwest` + chromiumoxide/外部 Chromium fallback；最轻现成进程是 Scrapling MCP + browser；最轻现成 REST 是 Crawl4AI `/crawl`。三者都没有成熟通用 `search`，因此生产配 SearXNG；Websurfx 仅作为 Rust-only/实验对照。若强制一个服务同时承载 `search + JS-heavy fetch`，self-host Firecrawl 仍是成熟基准；其 `/v2/search` 是上游搜索集成而非自有 web index，不能宣称“本地自有搜索”。`open-webSearch` 更轻但不保证 JS-heavy，Lightpanda 仅 Beta/WIP 实验，Browserless Search API 仅 Cloud。

## 需求语义分层

### A. In-process local fetch（本地执行、访问公网 URL）

输入是已经给出的公网 HTTP(S) URL；本地进程完成 DNS/HTTP/TLS、`robots.txt`、HTML 解析和正文抽取。它**不是搜索**，也不发现链接，不产生索引。Stravia 当前 `web_fetch` 正是这种 leaf：`FetchRequest.urls` 为 1–20 个 URL，`max_characters` 为每 URL 1,000–50,000，engine 以总字符上限公平收紧并逐个保留成功/错误状态（[`FetchRequest` 与限制](../../backend/crates/stravia-core/src/web_access/mod.rs#L126-L182)、[engine fetch](../../backend/crates/stravia-core/src/web_access/mod.rs#L527-L664)）。

### B. Local execution + remote search（本地服务/进程，依赖公网搜索服务）

SearXNG、远程 Exa/Brave/Tavily/Zhipu adapter 都属于这一类：调用者看见一个本地 HTTP/adapter seam，但召回来自外部 search service 或其 API/网页；不会因为“服务部署在本机”而变成自有索引。SearXNG 的 API 文档明确指出 `q` 会传给 external search services（[官方 Search API](https://docs.searxng.org/dev/search_api.html)）。

### C. Local metasearch service（本地元搜索）

这是 B 的可运维形态：自托管服务统一多个 engines、做去重/排序/格式化，但索引和 freshness 仍由上游负责。它可以满足“本地 endpoint、少量凭据、统一 JSON”，不能满足“断网可搜公网”或“Stravia 自己保存全网 corpus”。SearXNG 是首选代表；其默认 engine 清单同时列有 Brave、DuckDuckGo、Google CSE、Mojeek 等上游（[configured engines](https://docs.searxng.org/user/configured_engines.html)）。

### D. Fully local crawler + index（本地 crawler、正文、索引、查询）

本地系统要自己执行：seed/sitemap/link discovery、robots/rate policy、fetch/render、正文抽取、canonical/dedup、倒排/向量索引、recrawl、删除和查询。它必须声明 corpus 边界和 freshness；“全网”不是单机组件的默认能力。Tantivy 只提供索引层；Stract、YaCy 或 Spider+Tantivy 才接近完整 pipeline，但各自有 license、平台和运维代价。

### “搜索”在 Stravia 中的最小合理定义

建议把 `local search` 定义为：

- 一个**自有、版本化、可重建**的索引；
- corpus 由管理员明确的 seed/domain/sitemap/URL 文件限定；
- 每条结果带 canonical URL、抓取时间、index revision，必要时带正文抽取状态；
- 查询只承诺该 corpus 的相关性和 freshness，不声称覆盖实时公网；
- `web_search` 的 `mode = index` 与 `mode = agentic` 继续区分索引型结果和 agentic/provider 结果。

## 当前仓库缺口与接入 seam

### 已存在的 seam

- ADR-0002 将 Search 与 Fetch 收进独立 Web Access deep module；供应商细节被 `WebProviderAdapter` 隔离，Platform Tool、MCP 和 Responses adapter 复用同一 Web Access interface，不把模型循环、MCP 协议和供应商细节混成一个 abstraction（[ADR-0002](../adr/0002-web-access-provider-seam.md)）。
- `WebProviderAdapter` 目前只有 `provider_id`、`supports_search`、`supports_fetch`、`search(&SearchRequest)` 和 `fetch(&FetchRequest)`；成功值包在 `AdapterSuccess` 中，provider-native usage 不穿透公共 seam（[trait 定义](../../backend/crates/stravia-core/src/web_access/mod.rs#L405-L439)）。
- `WebAccessEngine` 按管理员顺序尝试 search provider；search 成功后做 domain post-filter；fetch 对仍失败的 URL 继续下一个支持 fetch 的 provider，并保持输入顺序。整次调用共享 60 秒 deadline，fetch 总字符限制为 64,000（[engine](../../backend/crates/stravia-core/src/web_access/mod.rs#L441-L665)、[ADR consequence](../adr/0002-web-access-provider-seam.md)）。
- 当前 provider 构造器已覆盖 Exa、Brave、Tavily、Zhipu 四种 config；Zhipu 同时使用 remote Search/Reader MCP endpoint（[providers.rs](../../backend/crates/stravia-core/src/web_access/providers.rs#L18-L60)）。
- Web Search 设计保留 Local 与 Codex 两个互不 fallback 的 backend；Local backend 的 hidden `web_access.search`/`web_access.fetch` leaves 仍复用现有 WebAccessService/provider priority/fallback/SSRF seam，而当前已存在的 Exa、Brave、Tavily、Zhipu 仍是 Search Sources（[Web Search backend](../design/web-search.md#5-backend)、[tool identity 与 surface](../design/web-search.md#4-tool-identity-与-surface)）。因此，当前仓库没有 native in-process fetch，也没有 local crawler/index；hidden leaf 仍可能依赖远程 provider。

### 配置与管理面缺口

现有 `WebProvider` 的字段形态是 `{id,name,kind,api_key,...}`，没有可供 self-hosted endpoint 使用的 `base_url/options`；Rust model 将 `api_key` 表示为 `Option<String>`，但 SQLite 与 PostgreSQL 最终 schema 都将其设为 `NOT NULL`，并只接受 `exa`、`brave`、`tavily`、`zhipu` 四种 kind；admin 同样要求这四种 kind 和非空 API key（[`WebProvider` model](../../backend/crates/stravia-core/src/db/models.rs#L445-L478)、[SQLite migration](../../backend/crates/stravia-core/migrations/sqlite/0010_web_research.sql#L53-L64)、[PostgreSQL migration](../../backend/crates/stravia-core/migrations/postgres/0010_web_research.sql#L48-L56)、[admin validation](../../backend/crates/stravia-core/src/admin/web_access.rs#L138-L167)）。因此：

- native HTTP provider 不能以 `api_key = None` 直接写入现有数据库；需要增加 local kind、调整 capability/admin 校验，并迁移两种数据库的 kind/credential constraints；
- SearXNG 与 YaCy 的 endpoint 不能硬编码为 localhost，也不能让用户提交任意 URL 后绕过 SSRF。必须新增经 scheme/host/port/私网/DNS policy 校验的 `base_url` 或等效 options；
- 这些变更涉及数据库/持久化 schema、admin API、WebUI 表单/校验、运行时 config snapshot 和 secret policy，**不是只加一个 adapter**；本研究不修改这些代码。

### 现有 SSRF 校验与 native fetch 的差异

当前 `validate_fetch_request` 在调用 provider 前：只接受 1–20 个 URL、`http`/`https`、无 username/password；拒绝 `localhost`、`.localhost`、`.local`、`home.arpa` 及其子域；拒绝字面量非公网 IP（[源码](../../backend/crates/stravia-core/src/web_access/mod.rs#L667-L720)）。对 hostname 通过 Tokio resolver 检查 A/AAAA，任何非公网答案都拒绝，并要求至少有一个答案（[源码](../../backend/crates/stravia-core/src/web_access/mod.rs#L721-L748)）。

这属于**入口校验**，并不能自动证明后续 HTTP 连接安全。reqwest 默认跟随最多 10 跳 redirect（[official redirect docs](https://docs.rs/reqwest/latest/reqwest/redirect/index.html)）；每次 redirect 可能变更 host、scheme 或 DNS。native fetch PoC 必须：

1. 对每个 redirect 的目标 URL 重新做 scheme/credential/hostname/IP policy；
2. 重新解析并检查所有 A/AAAA，不能只信第一次解析；
3. 让实际连接使用已检查的解析结果，或提供等价 resolver/connection pinning，避免 DNS rebinding/TOCTOU；
4. 明确 HTTP proxy 是否启用、`NO_PROXY`/管理员 proxy policy 是否允许绕过校验；reqwest 官方 `ClientBuilder` 提供 proxy、no_proxy、custom resolver、`resolve`/`resolve_to_addrs` 和 timeout（[ClientBuilder](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)）；
5. 对 redirect 数、跨 scheme、跨 domain、响应体、压缩后大小和下载时间设置独立上限。

## Rust `web_fetch` 候选

### 传输、URL 与 robots

| 候选 | 官方事实与能力 | license / 维护信号 | 运行与边界 | 结论 |
|---|---|---|---|---|
| [`reqwest`](https://github.com/seanmonstar/reqwest) | Async/blocking client、JSON、压缩、cookie、proxy、TLS、WASM；`ClientBuilder` 有 total/read/connect timeout、redirect policy、HTTPS-only、custom resolver/address override。默认 redirect 最多 10 跳（[README](https://github.com/seanmonstar/reqwest)、[ClientBuilder](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)、[redirect](https://docs.rs/reqwest/latest/reqwest/redirect/index.html)）。 | MIT OR Apache-2.0；官方仓库有 CI；不能据此承诺 SLA。 | 默认 rustls；`native-tls` 可用 Windows/macOS 系统 TLS，Linux 可能依赖 OpenSSL（[README requirements](https://github.com/seanmonstar/reqwest#requirements)）。不负责 robots、正文抽取、crawler 或 index。 | **近期首选传输层**；关闭/定制 redirect，显式决定 proxy/DNS policy。 |
| [`url`](https://docs.rs/url/latest/url/) | rust-url 实现 WHATWG URL Standard；parse 返回 `Result`，有 scheme/host/port/path、base URL `join`，可选 serde；可用 `alloc` no_std feature（[官方 docs.rs](https://docs.rs/url/latest/url/)）。 | MIT OR Apache-2.0；官方 Servo rust-url 仓库及 Cargo metadata 可核验。 | 纯 parser，无网络访问；不能替代公网/私网 IP 判定。 | **首选 URL 归一化/redirect join**；与 SSRF policy 一起使用。 |
| [`robotstxt`](https://docs.rs/robotstxt/latest/robotstxt/) | Google robots.txt parser/matcher 的 native Rust port；无第三方依赖，说明保留原库行为并通过 Google original tests（[docs.rs](https://docs.rs/robotstxt/latest/robotstxt/)、[crate page](https://docs.rs/crate/robotstxt/latest)）。 | Apache-2.0；0.3.0 发布于 2021-02-13，来源显示长期无新 release，维护活跃度只能评为低/未知。 | parser/matcher，不负责取得 `/robots.txt`、缓存、HTTP 错误或并发；由调用方落实 RFC 9309 fetch/cache/error policy。 | **可做 PoC 候选但需固定版本、补 RFC 9309 合规测试**。 |
| [`robots_txt`](https://docs.rs/robots_txt/latest/robots_txt/) | 轻量 parser/generator，支持 User-agent、Disallow、Allow、Crawl-delay、Request-rate、Sitemap、Host（[docs.rs](https://docs.rs/robots_txt/latest/robots_txt/)）。 | MIT OR Apache-2.0；0.7.0 发布于 2020-03-31；README 自称 implementation 是 WIP（[crate page](https://docs.rs/crate/robots_txt/latest)）。 | 无网络 fetch/cache；语义与 RFC 9309 差异需自行测试。 | **不作为默认选择**；仅在对扩展字段有明确需求时比较。 |
| [RFC 9309](https://www.rfc-editor.org/rfc/rfc9309) | IETF Standards Track 的 Robots Exclusion Protocol；说明 robots 是 crawler 应遵守的访问约束，**不是 access authorization**，并规定 redirect、unavailable/unreachable、parsing error、caching 和 limits（[全文](https://www.rfc-editor.org/rfc/rfc9309)）。 | 标准，无软件 license。 | 需把 robots decision、缓存 TTL、错误策略、UA identity 和 rate policy 作为 WebAccess policy。 | **规范基线**；parser 必须用 RFC fixture 验收。 |

### HTML、正文抽取与动态页面

| 候选 | 官方事实与能力 | license / 维护信号 | 平台/依赖与限制 | 结论 |
|---|---|---|---|---|
| [`scraper`](https://github.com/rust-scraper/scraper) | 基于 Servo `html5ever` 与 `selectors` 的 browser-grade HTML parsing、CSS selector、DOM text；可 parse document/fragment 和提取 element text（[docs.rs](https://docs.rs/scraper/latest/scraper/)、[官方仓库](https://github.com/rust-scraper/scraper)）。 | 官方 Cargo manifest 标注 ISC；有 GitHub test workflow。 | 解析已取得 HTML；没有 JS runtime、网络、crawler 或 index。纯 Rust 依赖，具体 target matrix 仍应在 Stravia CI 验证。 | **静态 DOM 必选 building block**；不是正文抽取完整答案。 |
| [`dom_smoothie`](https://github.com/niklak/dom_smoothie) | Rust crate，紧跟 Mozilla `readability.js`；可输出 `Article` 的 title/byline/excerpt/content/text、metadata，支持 Markdown/formatted text；README 明确 text 结果“far from perfect”，仍可能丢内容或保留噪声（[docs.rs crate](https://docs.rs/crate/dom_smoothie/latest)、[GitHub README](https://github.com/niklak/dom_smoothie)）。 | MIT；crate 页面显示 0.18.0 发布于 2026-06-07，仓库有 Rust/coverage/benchmark/release workflows，维护信号较强。 | 处理 HTML/DOM，不执行 JS；依赖 `dom_query` 等 Rust crate；质量必须用目标语言、新闻/文档/表格 fixture 验证。 | **近期首选正文抽取**，需保留 fallback 与 limitations。 |
| [`readability`](https://docs.rs/crate/readability/latest) | arc90 Readability 的 Rust port，提供 primary readable content 的 HTML/text；可直接从 URL scrape（[crate page](https://docs.rs/crate/readability/latest)、[docs.rs API](https://docs.rs/readability/latest/readability/)）。 | MIT；0.3.0 发布于 2023-12-20，docs coverage 0%，维护活跃度偏低/未知。 | 依赖较旧 `html5ever`/可选 `reqwest 0.11`；URL fetch 与 extraction 混合，不适合直接承接 Stravia SSRF/timeout/redirect policy。 | **备选/对照实现**，不作为首个生产依赖。 |
| [`headless_chrome`](https://docs.rs/headless_chrome/latest/headless_chrome/) | Rust high-level CDP API；可执行 JavaScript、等待 DOM、拦截 network、截图/PDF，并可下载 known-good Chromium binaries for Linux/Mac/Windows（[官方 docs.rs](https://docs.rs/headless_chrome/latest/headless_chrome/)、[Cargo manifest](https://raw.githubusercontent.com/rust-headless-chrome/rust-headless-chrome/master/Cargo.toml)）。 | MIT；manifest 可核验版本 1.0.22、Rust 1.85；官方仓库有 CI/release 信号。 | 启动/连接 Chrome/Chromium 外部进程；browser binary、sandbox、profile、启动耗时、内存和容器都要运营。 | **动态 fallback 候选**；只对明确需要 JS 的 URL 开启。 |
| [`chromiumoxide`](https://github.com/mattsse/chromiumoxide) | 高层 async CDP API，可 launch 或连接 headless/full Chrome/Chromium，控制导航、点击、等待 navigation、取 HTML（[官方 README](https://github.com/mattsse/chromiumoxide)、[docs.rs](https://docs.rs/crate/chromiumoxide/latest)）。 | Apache-2.0 OR MIT；官方仓库有 CI；docs.rs 记录 2026 release。 | 只支持 Tokio；默认寻找已安装 Chromium，也可用 fetcher 下载部分平台 binary；不是内置 JS engine。容器/Windows 的 Chrome 安装与 sandbox 需 PoC。 | **与 Tokio core 接近的 CDP 备选**；与 headless_chrome 二选一。 |
| [Playwright](https://playwright.dev/docs/languages) | 官方支持 JavaScript/TypeScript、Python、Java、.NET；官方语言页不列 Rust（[Supported languages](https://playwright.dev/docs/languages)）。 | 各语言包 license/版本需分别核验，不能当 Rust dependency。 | 适合 sidecar/service，但引入 Node/Python/Java/.NET runtime、浏览器安装和跨进程边界。 | **仅在需要成熟浏览器生态时作为 sidecar**。 |
| [`spider`](https://github.com/spider-rs/spider) | MIT Rust crawler/scraper；local mode 不需要 key；HTTP-first，JS-heavy 页面可开启 `features = ["chrome"]` 后 `crawl_smart()`；内置 depth/limit/delay/robots/subdomain/stealth，并支持 Markdown/JSON/WARC 输出（[官方 README](https://github.com/spider-rs/spider)、[crate metadata](https://docs.rs/crate/spider/latest)）。 | MIT；crate 页面显示 2.53.4 于 2026-07-30 发布，仓库有 CI/audit/bench/release workflows。 | HTTP 路径较可移植；Chrome/cache/remote/distributed optional features 增加系统依赖；官方 reviewed sources 未承诺完整 Windows/容器矩阵。它会发现链接。 | **后续 crawler/站点采集 PoC**；当前明确 URL leaf 过重。 |

### 推荐组合与边界

```text
WebAccessEngine policy
  -> reqwest::Client（TLS/timeout/proxy/redirect policy）
  -> url::Url（parse + redirect join + canonicalization）
  -> GET /robots.txt + robotstxt matcher（RFC 9309 policy）
  -> bounded response bytes / content-type / charset
  -> scraper/html5ever（DOM）
  -> dom_smoothie（article/title/text/metadata）
  -> FetchResult（按原 URL 顺序、状态、截断与错误）
```

这是选型边界，不是待复制实现。必须避免 `readability::extractor::scrape(url)` 这种绕过统一 SSRF、timeout、redirect 和 body limit 的快捷路径。动态页面先取 HTTP HTML；只在静态正文为空或显式站点 policy 要求时调用 browser；browser 仍复用 URL/domain/egress policy，不得把页面凭据、父模型 secrets 或任意本地文件暴露给不可信页面。
## 轻量 JS-heavy 替代（比 Firecrawl 更轻？）

### 先把“轻量”拆成可核验维度

这里的“轻量”不是未经测量的内存或吞吐承诺，而是把可观察的运行边界拆开：**常驻服务/进程数量、是否引入数据库或队列、是否必须有浏览器、语言/runtime、是否有现成 HTTP API、正文/Markdown 抽取、JS/交互能力、license，以及 Windows/Docker 证据**。下表只记录官方仓库、manifest、官方文档或 license 能直接证明的事实；没有资源 benchmark 就不写 pages/s、内存或“比 X 少多少”。

### 候选矩阵（只对应 `WebProviderAdapter::search/fetch` leaf）

| 候选 | 类型 / runtime | `search` | API、正文/Markdown | JS / 交互 | 常驻进程、DB/queue | license；Windows / Docker 证据 | 判断 |
|---|---|---:|---|---|---|---|---|
| `reqwest` + `scraper`/`dom_smoothie` + [`chromiumoxide`](https://github.com/mattsse/chromiumoxide) | Rust in-process；chromiumoxide 是 Tokio async CDP library，控制外部 Chrome/Chromium | 否，需配 SearXNG/Websurfx | 没有现成 HTTP API；CDP 是浏览器控制通道，不是 provider REST。HTML/正文/Markdown 需由 Stravia 自己抽取；chromiumoxide README 展示导航、点击、等待和取 HTML（[README](https://github.com/mattsse/chromiumoxide)、[manifest](https://raw.githubusercontent.com/mattsse/chromiumoxide/main/Cargo.toml)）。 | 有：浏览器执行页面 JS，能做导航/点击/等待；等待策略、抽取和动作白名单由调用方实现 | library 不带 DB/queue；实际是 Stravia 进程加按需启动/连接的 Chrome 进程（可复用 browser）。fetcher 是可选下载器，不能把浏览器成本变成零 | MIT OR Apache-2.0；manifest 有 Windows registry dependency，fetcher 源码列出 Win32/Win64（[platform.rs](https://raw.githubusercontent.com/mattsse/chromiumoxide/main/chromiumoxide_fetcher/src/platform.rs)）；容器/Chrome sandbox 需 Stravia PoC | **三档中的最轻 Rust 集成**；优先 native fast path，只有 pending JS URL 才 browser fallback |
| [`headless_chrome`](https://github.com/rust-headless-chrome/rust-headless-chrome) | Rust synchronous CDP library；官方 README 称其为 Puppeteer 的 Rust equivalent | 否，需配 SearXNG/Websurfx | 没有 search/fetch REST；页面 HTML/text/截图/PDF/网络拦截能力由 CDP API 提供，文章正文/Markdown仍需另接抽取器 | 有：执行 JS、等待 DOM、点击/键盘和 network interception；同步 API、plain threads | library 不带 DB/queue；实际会启动/连接 Chrome/Chromium 进程；官方 `fetch` feature 可下载 known-good Chromium | MIT；README 明列 Linux/Mac/Windows binary download（[README](https://github.com/rust-headless-chrome/rust-headless-chrome)；[manifest](https://raw.githubusercontent.com/rust-headless-chrome/rust-headless-chrome/master/Cargo.toml)）；Docker/沙箱需自行验收 | 与 chromiumoxide 二选一；Stravia Tokio core 更偏向 chromiumoxide，不能同时引入两套 |
| [`Scrapling`](https://github.com/D4Vinci/Scrapling) MCP | Python >=3.10 library/process；静态 Fetcher、Playwright Dynamic/StealthyFetcher、可连已有 CDP（[动态 fetcher](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/docs/fetching/dynamic.md)） | **否**。MCP server 注册的是 `get`/`fetch`/`stealthy_fetch`/session/screenshot；`google_search` 参数只是 referer 设置，不是 search tool（[server source](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/scrapling/core/ai.py)） | `Response`/MCP `ResponseModel` 可返回 Markdown/HTML/text；有 MCP transport，不是 REST。`serve(http=False)` 是 stdio；`serve(http=True)` 明确使用 MCP `transport="streamable-http"`（[MCP API reference](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/docs/api-reference/mcp-server.md)、[serve source](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/scrapling/core/ai.py)） | Dynamic/Stealthy 使用 Chromium/Chrome + Playwright，支持 wait selector、page action、CDP URL、XHR capture；静态 Fetcher 不需浏览器 | 进程形态可收敛为一个 Python MCP process + 按需/复用的 browser process；manifest 的核心/可选依赖没有 DB/queue，session 状态在进程内。官方 Dockerfile 安装 Chromium 并暴露 MCP 端口，但不能因此称 REST service | BSD-3-Clause；pyproject classifier 为 OS Independent；官方 Dockerfile/README 提供 Docker 镜像和 Playwright Chromium，native Windows 细节需 Stravia CI 验证（[manifest](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/pyproject.toml)、[Dockerfile](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/Dockerfile)） | **三档中的最轻现成进程**；`web_search` 必须另接 SearXNG/Websurfx；adapter 要讲 MCP 协议，不得伪称 `/fetch` REST |
| [`Crawl4AI`](https://github.com/unclecode/crawl4ai) package / Docker REST | Python >=3.10 crawler；Playwright/Patchright；官方 Docker 是 FastAPI server | **无通用 search API**。仓库另有 `GoogleSearchCrawler`，它在浏览器中抓 Google SERP 并抽取字段，不是自有 index 或稳定通用 search provider（[source](https://raw.githubusercontent.com/unclecode/crawl4ai/main/crawl4ai/crawlers/google_search/crawler.py)） | 现成 REST：`POST /crawl`、`/crawl/stream`，另有 `/html`、`/execute_js` 等；README/部署文档说明 Markdown 与结构化抽取（[Docker guide](https://raw.githubusercontent.com/unclecode/crawl4ai/main/deploy/docker/README.md)）。 | Playwright Chromium、等待/JS/configurable actions；适合 browser crawler，不是只做单 URL fetch 的薄库 | pip library 可直接 `AsyncWebCrawler`，不应把 Docker 组件误算为必需；官方 Dockerfile 同时安装 FastAPI app、Playwright Chromium、Redis server/tools 和 supervisor，且 API 提供 `/crawl/job` 后台 job/webhook，因此 REST image 有额外内部进程和 queue；pyproject 还声明 `aiosqlite`（[Dockerfile](https://raw.githubusercontent.com/unclecode/crawl4ai/main/Dockerfile)、[manifest](https://raw.githubusercontent.com/unclecode/crawl4ai/main/pyproject.toml)） | Apache-2.0；官方部署文档给 Docker、`linux/amd64`/`linux/arm64`，并明确 prerequisites 至少 4GB RAM；未从 reviewed sources 得到 native Windows matrix（[Docker guide](https://raw.githubusercontent.com/unclecode/crawl4ai/main/deploy/docker/README.md)） | **三档中的最轻现成 REST**（不是资源 benchmark）：单容器 FastAPI `/crawl` + SearXNG；不因为“单容器”就宣称比 Firecrawl 省内存 |
| [`Lightpanda`](https://github.com/lightpanda-io/browser) | 独立 Zig headless browser（非 Chromium fork）；V8 + libcurl + html5ever | 否；没有 search/index/provider API | CLI `fetch --dump html\|markdown` 可直接导出 Markdown；`serve` 提供 CDP/WebSocket，非通用 fetch REST；DOM/JS 结果交给 CLI/CDP client | 有 JS/DOM/Ajax、click/input、network interception、wait selector/script；官方 status 明确 Beta/WIP、coverage 仍在增加且可能 crash（[README status/features](https://raw.githubusercontent.com/lightpanda-io/browser/main/README.md)） | 单一 browser process/CLI 或 CDP server；官方 reviewed sources 未见 DB/queue | AGPL-3.0；**无 native Windows binary，Windows 需 WSL2**；官方 Docker image 只列 Linux amd64/arm64（[README](https://raw.githubusercontent.com/lightpanda-io/browser/main/README.md)、[LICENSE](https://raw.githubusercontent.com/lightpanda-io/browser/main/LICENSE)） | 不作近期生产 fallback；仅在可接受 AGPL/WSL2 且先验证目标站兼容性的实验中比较 |
| [`Browserless`](https://github.com/browserless/browserless) | TypeScript browser service；Docker/CDP 与 REST routes | README 列 `/search`，但官方 Search API 明确 **仅 Cloud plans**；不能把 self-host OSS 当作本地 search（[Search API](https://docs.browserless.io/rest-apis/search)） | self-host 的核心是 CDP；README 列 `/smart-scrape`、`/crawl` 等 REST，正文格式需按各 route 验收 | Chrome/Chromium 等 browser backends、并发/queueing；适合 browser service | Docker service；README 宣称 parallelism/queueing，但本研究不把其云端持久化或资源配置推断为本地依赖 | SPDX `SSPL-1.0 OR Browserless Commercial License`；商业闭源/CI 使用需 commercial license（[README licensing](https://raw.githubusercontent.com/browserless/browserless/main/README.md)）；官方 Docker/ghcr，native Windows 不作为证据 | API/license 虽清楚，但 search cloud-only 且 license gate 明显；**不纳入“轻量本地开源”推荐** |

### 三档推荐与问题的直接答案

1. **最轻 Rust 集成：** `reqwest` native fast path + `chromiumoxide`（或外部 Chromium/CDP）fallback。没有 DB/queue，也不需要常驻 browser：普通 URL 只走 HTTP；只有 SPA shell、正文为空/过短或 domain policy 明确要求 render 时才启动/复用浏览器。代价是 Stravia 必须自己实现 wait/抽取/Markdown、SSRF/DNS pinning、redirect、robots、body/CPU/time budget 和 browser egress isolation；chromiumoxide/headless_chrome 不是现成 provider。
2. **最窄的现成 service 边界：** Scrapling MCP service + Playwright/Chromium child/session，`search` 明确为 false，另配 SearXNG（或 Websurfx）。默认 MCP 是 stdio；需要 HTTP 时启用 `serve(http=True)`，它是 MCP streamable HTTP/JSON-RPC，不是 `POST /fetch` REST。没有数据库/队列的官方证据，session 状态在 service 进程内；浏览器进程成本仍然存在。
3. **最轻现成 REST：** Crawl4AI 单容器 FastAPI `/crawl`（或 `/crawl/stream`）+ SearXNG。它有现成 HTTP API 和 Markdown/结构化抽取，但官方部署前提至少 4GB RAM，Dockerfile 还带 Playwright、Redis、supervisor；“单容器”只是部署边界，**不是比 Firecrawl 资源更轻的 benchmark 证据**。无通用 `search` API，GoogleSearchCrawler 不能替代 SearXNG。

**Firecrawl 有没有更轻的替代？** 有，但必须拆分：只做 `web_fetch` 时，Rust integration 可去掉 sidecar/DB/queue，Scrapling 可收窄为 MCP service + browser，Crawl4AI 可提供比完整双工具 stack 更窄的 fetch-only REST 边界（但其 Docker 仍含 Redis/supervisor，不能据此声称资源更少）。截至本次官方来源核验，**不存在一个成熟、单服务、同时提供 search 与 JS-heavy fetch 且可明确证明比 Firecrawl 更轻的本地开源方案**：Scrapling/Crawl4AI/Lightpanda 没有通用 search；Browserless search 是 Cloud-only；Rust libraries 不是 service。Firecrawl 的 `/v2/search` 也只是其 search route/上游搜索集成，**不是自有 web index**。


## Rust / 自托管 `web_search` 候选

| 候选 | 类型与索引 | 依赖、license、维护 | Windows/容器/抽取 | 适配判断 |
|---|---|---|---|---|
| [`Tantivy`](https://github.com/quickwit-oss/tantivy) | Rust in-process full-text index library；自有本地索引 building block，**没有** crawler、URL discovery、robots、web fetch 或 search service。官方明确不是 off-the-shelf server；stable Rust 支持 Linux/macOS/Windows（[README](https://github.com/quickwit-oss/tantivy)、[docs.rs](https://docs.rs/tantivy/latest/tantivy/)）。 | MIT；官方 test/coverage/security workflow。 | Windows 有 README 证据；容器由应用自行封装；无正文抽取。 | **后续 local index 内核**，不是单独的 `web_search`。 |
| [`Stract`](https://github.com/StractOrg/stract) | Rust 完整 web search engine；README 声明 fully independent index、own crawler、query syntax，Tantivy 提供 inverted index（[README](https://github.com/StractOrg/stract)）。 | AGPL-3.0（[LICENSE](https://raw.githubusercontent.com/StractOrg/stract/main/LICENSE.md)）；截至本日 GitHub API `archived: true`，最后 push `2025-03-24T08:42:45Z`（[API](https://api.github.com/repos/StractOrg/stract)）。 | setup 要 Rust、clang、npm、liburing（Linux 示例）、just、wasm-pack、Python venv/local index；官方 sources 未承诺 Windows 或 container matrix（[CONTRIBUTING](https://raw.githubusercontent.com/StractOrg/stract/main/CONTRIBUTING.md)）。 | **架构参考/受控实验**，不作新生产依赖。 |
| [`SearXNG`](https://github.com/searxng/searxng) | Python self-hosted metasearch；**不自有 web index**，聚合 external search services/databases；API JSON/CSV/RSS（需启用），`q` 明确传给 external services（[README](https://github.com/searxng/searxng)、[Search API](https://docs.searxng.org/dev/search_api.html)）。 | AGPL-3.0；有 container/integration/documentation/data-update workflows。 | 官方推荐 Docker/Podman Compose，Docker Hub/GHCR，config/cache volumes 与可选 Valkey（[container install](https://docs.searxng.org/admin/installation-docker.html)）；无上游 native Windows 支持，但本研究已用窄 patch 验证 Windows 下游 build 可运行；无通用正文抽取 API。 | **近期公网 search endpoint 首选**；合同必须明示 upstream dependency、rate-limit、provenance。 |
| [`YaCy`](https://github.com/yacy/yacy_search_server) | Java 完整 search/index/crawler/web UI；README 明确 server hosting search index、web frontend、production-ready crawler/scheduler；可退出 peer network、local index only（[README](https://github.com/yacy/yacy_search_server)）。 | README 说明 GPL 2.0 or later，部分元素 LGPL；仓库 API 截至本日 `archived:false`、`pushed_at:2026-08-16T19:53:16Z`（[API](https://api.github.com/repos/yacy/yacy_search_server)）。 | Java 17+、Ant；Windows installer 构建和 Docker image `yacy/yacy_search_server:latest`；有 index/search crawler parser，但本研究未确认独立 article extraction API。 | **跨语言完全本地/内网 PoC 首选**；限 seed/domain、关闭 P2P，不承诺全网。 |
| [`Apache Nutch`](https://github.com/apache/nutch) | Java extensible/scalable crawler；可 single machine，也可 Hadoop；索引写入通过 index-writer/plugin，非现成 search UI（[README](https://github.com/apache/nutch)）。 | Apache-2.0；Apache CI/Jenkins smoke-test。 | 官方 Dockerfile/README；Java/Ant/plugin 配置；Windows native 矩阵未确认，正文抽取依赖 parse plugins（[Docker README](https://raw.githubusercontent.com/apache/nutch/master/docker/README.md)）。 | **大规模 crawler 参考/受控 pipeline**；查询还要另接 index server。 |
| `Spider + Tantivy` | Rust 组合：Spider bounded crawl/fetch，Tantivy own index；需要自建 canonical/dedup、schema、commit/merge、recrawl/delete、ranking、query/provenance。 | Spider/Tantivy 均 MIT；optional Chrome feature 需锁版本和系统依赖。 | 可容器化但完整 Chrome/platform matrix 要 PoC；无现成完整 search service。 | **Rust-only 长期路线**，不替代当前 fetch leaf。 |

## SearXNG 与 Websurfx：`web_search` 逐项核验矩阵

两者都必须标为 **metasearch**：SearXNG README 将自身定义为 metasearch engine，Websurfx rolling README 也明确写作 meta search engine；二者都把 query 发送给配置的上游 engines，**都没有自有 web index**（[SearXNG README](https://github.com/searxng/searxng)、[SearXNG Search API](https://docs.searxng.org/dev/search_api.html)、[Websurfx README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)）。

| 维度 | SearXNG | Websurfx（rolling） | 对 Stravia 的含义 |
|---|---|---|---|
| 实现语言 / runtime | Python 应用（官方 `requirements.txt`；源码使用 Flask/HTTPX），容器文档提供官方 image 与 Compose（[`requirements.txt`](https://raw.githubusercontent.com/searxng/searxng/master/requirements.txt)、[`webapp.py`](https://raw.githubusercontent.com/searxng/searxng/master/searx/webapp.py)、[container install](https://docs.searxng.org/admin/installation-docker.html)）。 | Rust binary，Cargo edition 2024；Actix Web 4.11、Tokio 1.47、Reqwest 0.12（[`Cargo.toml`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/Cargo.toml)）。 | SearXNG 是 Python sidecar；Websurfx 符合 Rust-only 偏好，但并不因此证明吞吐、内存或稳定性更好。 |
| license / 发布与维护信号 | AGPL-3.0-or-later（源码 SPDX/README）；采用 rolling release，版本来自 git commit，官方文档当前构建为 `2026.8.16+b2da6b90f`（[LICENSE](https://raw.githubusercontent.com/searxng/searxng/master/LICENSE)、[CHANGELOG](https://raw.githubusercontent.com/searxng/searxng/master/CHANGELOG.rst)、[docs version](https://docs.searxng.org/dev/search_api.html)）。 | `Cargo.toml` 与 README 标注 AGPL-3.0；`v1.29.9` release 页面记录为 13 May，rolling 分支仍使用 1.29.9；README 称已可生产使用，同时写明征集贡献者、roadmap “Coming soon”（[Cargo.toml](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/Cargo.toml)、[release v1.29.9](https://github.com/neon-mmd/websurfx/releases/tag/v1.29.9)、[README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)）。 | 维护信号支持 SearXNG 作为生产默认；Websurfx 的 release/rolling 与文档一致性需在 PoC 中锁版本，不能以项目自称的 “lightning-fast” 推断质量。 |
| 部署 / 依赖 | 官方推荐 Docker 或 Podman Compose；官方镜像有 Docker Hub/GHCR；Compose 示例包含 core 与可选 Valkey，配置挂载 `/etc/searxng`、缓存数据挂载 `/var/cache/searxng`（[container install](https://docs.searxng.org/admin/installation-docker.html)、[Valkey settings](https://docs.searxng.org/admin/settings/settings_valkey.html)）。 | 源码构建需要 Cargo；README 的 bare-metal 步骤单独启动 Redis；官方 Dockerfile 是 multi-stage、`scratch` runtime，`CACHE=memory\|redis\|hybrid\|no-cache` 选择 feature/构建，Redis 只对 redis/hybrid 必需（[README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)、[Dockerfile](https://github.com/neon-mmd/websurfx/blob/rolling/Dockerfile)、[Cargo features](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/Cargo.toml)）。 | SearXNG 的 Compose 运维路径更成熟；Websurfx 可做单二进制/轻容器，但启用 Redis cache 会增加依赖。 |
| 上游 engine 生态 / 配置 | 官方配置文档列出 270 个 engines、83 个默认启用；每 engine 可设置 `categories`、`timeout`、`weight`、`api_key`、`retries`、`proxies`、`retry_on_http_error`、HTTP/2 等（[configured engines](https://docs.searxng.org/user/configured_engines.html)、[engine settings](https://docs.searxng.org/admin/settings/settings_engines.html)）。 | 源码 registry 当前支持 `duckduckgo`、`searx`、`brave`、`startpage`、`librex`、`mojeek`、`bing`、`wikipedia`、`yahoo`、`qwant`、`sepiasearch`；Lua `upstream_search_engines` 仅开关这些名称（[`engine.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/engine.rs)、[`config.lua`](https://github.com/neon-mmd/websurfx/blob/rolling/websurfx/config.lua)）。 | SearXNG 覆盖面与 engine-specific filters/config 更丰富；Websurfx 新增 upstream 需 Rust engine/parser 代码，不是只改 JSON。 |
| 并发 / 聚合 / 失败 | 每个选中 engine 启动线程并行请求；共享实际 timeout，超时或异常写入 `unresponsive_engines`，仍返回其余结果；JSON 也保留该字段（[`search/__init__.py`](https://raw.githubusercontent.com/searxng/searxng/master/searx/search/__init__.py)、[`webutils.py`](https://raw.githubusercontent.com/searxng/searxng/master/searx/webutils.py)）。 | `aggregator.rs` 用 Tokio `JoinSet` 并发抓取，Rayon 去重/TF-IDF rerank；`SearchResults.engineErrorsInfo` 保留 error/engine/severity。当前源码按 task 完成顺序 `names.pop()` 关联 engine，且重复结果调用 `value.1.to_owned().add_engines(...)` 修改 clone；因此 provenance 合并/归属必须 PoC 验收，不能直接宣称可靠（[`aggregator.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/aggregator.rs)、[`aggregation.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/aggregation.rs)）。 | 两者都支持部分结果/failure normalization；Websurfx adapter 必须先验证 engine provenance，不可盲信字段。 |
| HTTP / JSON 契约 | `GET\|POST /` 与 `/search`；GET query 参数、POST form；`format=json\|csv\|rss` 必须在 `search.formats` 启用，否则 403。`q`、`categories`、`language`、`pageno`、`time_range`、`safesearch` 等有官方文档。JSON 顶层含 `query`、`results`、`answers`、`corrections`、`infoboxes`、`suggestions`、`unresponsive_engines`（[Search API](https://docs.searxng.org/dev/search_api.html)、[`webutils.get_json_response`](https://raw.githubusercontent.com/searxng/searxng/master/searx/webutils.py)）。 | 只有 `/search` GET route；源码 `SearchParams` 是 `q`、`page`、`safesearch`、`json`，`json=true` 返回 JSON，空 query/解析失败返回 JSON 400。README 示例却写成 `format=json=true`，而 `format` 并非 `SearchParams` 字段；官方没有独立稳定 API 文档，需以 rolling 源码和 smoke test 锁定（[`search.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/routes/search.rs)、[`search_route.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/search_route.rs)、[README JSON 示例](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)）。 | SearXNG 可直接写稳定 adapter；Websurfx 的 query/JSON 契约属于 PoC 风险，不能照 README 的 `format` 示例实现。 |
| filters / provenance | `categories`、`language`、`time_range`、`safesearch` 是否生效取决于 engine capability；JSON result 按 engine 结果序列化，失败列在 `unresponsive_engines`，不会变成 Stravia 自有 corpus provenance（[Search API](https://docs.searxng.org/dev/search_api.html)、[configured engines](https://docs.searxng.org/user/configured_engines.html)）。 | URL、title、description、`engine`、`relevanceScore` 与安全/过滤标志被序列化；safe search 0–4 及 block/allow list 在 config/source 中实现，engine error 单独输出（[`aggregation.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/aggregation.rs)、[`configuration.md`](https://github.com/neon-mmd/websurfx/blob/rolling/docs/configuration.md)）。 | adapter 应把 engine 名称作为 upstream provenance，并明确 “remote/metasearch”；不要映射为 `mode: Index`。 |
| 缓存 / 限流 / proxy | SearXNG 的 limiter 依赖 Valkey，可按 IP/行为做 bot/rate protection；outgoing 支持 request timeout、连接池、多个 proxy round-robin、Tor、重试和 redirect 上限。搜索 API 本身不承诺结果缓存语义（[limiter](https://docs.searxng.org/admin/searx.limiter.html)、[outgoing](https://docs.searxng.org/admin/settings/settings_outgoing.html)）。 | 默认 `memory-cache`（Moka TTL）；可选 `redis-cache`，并有压缩/加密 cache features；默认配置 `cache_expiry_time=600`、HTTP cache header `60s`；Actix Governor 默认 `20` requests / `3s`，单一 reqwest proxy 可配置（[`Cargo.toml`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/Cargo.toml)、[`memory.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/cache/memory.rs)、[`redis.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/cache/redis.rs)、[`config.lua`](https://github.com/neon-mmd/websurfx/blob/rolling/websurfx/config.lua)、[`lib.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/lib.rs)）。 | SearXNG 的限流/多 proxy/engine suspension 更可配置；Websurfx cache 更直接但需确认 cache key/TTL 与 Stravia deadline 的交互。 |
| 管理 / 安全边界 | `settings.yml` 可启用/禁用 JSON formats、private engine tokens、plugins；limiter 要求正确 `X-Forwarded-For`/`X-Real-IP`，可用 Valkey。插件配置支持默认 active/opt-in/opt-out（[plugins](https://docs.searxng.org/admin/settings/settings_plugins.html)、[limiter](https://docs.searxng.org/admin/searx.limiter.html)、[engine settings](https://docs.searxng.org/admin/settings/settings_engines.html)）。 | server 默认绑定 `127.0.0.1`；源码启用 GET-only CORS `allow_any_origin` 与 Governor，但未见 API auth middleware；用户可配置 `binding_ip`，因此 adapter 仍须 loopback/endpoint policy（[`config.lua`](https://github.com/neon-mmd/websurfx/blob/rolling/websurfx/config.lua)、[`main.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/main.rs)、[`lib.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/lib.rs)）。 | 两者都不是 Stravia 的 SSRF 安全边界；只允许受控本机 endpoint，关闭不必要 UI/插件/跨域暴露并在 adapter 做 timeout/error normalization。 |
| Windows / Docker | 官方支持和文档仍以 Linux/容器为主；没有 native Windows 支持矩阵。2026-08-17 在 Windows 11 x64 对 commit [`b2da6b90f`](https://github.com/searxng/searxng/commit/b2da6b90f2f8446557c91f67d6be5064ab785ecd) 做了本地运行 PoC：原样源码不能直接运行，但以两个 Windows compatibility patch、`tzdata` 和 JSON 配置修补后，Flask `/healthz`、Granian WSGI、DuckDuckGo JSON 与 `sogou wechat` JSON 均实际成功；这证明“可维护下游 Windows build”，**不等于上游原生支持**。官方 Docker/Podman 证据充分（[container install](https://docs.searxng.org/admin/installation-docker.html)）。 | README 只承诺 `x86_64`；官方 Dockerfile 有 scratch image，CI 仅 `ubuntu-latest`，未提供 native Windows matrix，故 Windows 原生支持未知（[README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)、[Dockerfile](https://github.com/neon-mmd/websurfx/blob/rolling/Dockerfile)、[CI](https://github.com/neon-mmd/websurfx/blob/rolling/.github/workflows/rust.yml)）。 | Windows 无前置依赖发行不能直接照搬官方容器；SearXNG 需要 Stravia 维护独立的资源目录、patch、Windows CI 和验收。 |
| Stravia adapter 工作量 / 选择 | HTTP/JSON 字段和 filters/provenance 有正式 Search API；主要工作是 base URL 校验、格式启用、engine provenance/错误映射和超时策略。 | Rust sidecar 本身不需 Python，但 JSON route 没有独立稳定文档，README/source 参数矛盾，且 provenance 需验收；需要维护 rolling pin、schema fixture 和错误映射。 | **生产默认选 SearXNG**；**Rust-only/实验选 Websurfx**；若必须判断两者召回、engine 可用性、真实延迟、Windows 原生运行或资源占用，必须做 PoC，本文不作 benchmark 推断。 |

文档不能判断二者的相关性、吞吐、内存、稳定性、上游封禁率或长期 engine 可用性；这些只能用固定 engine 配置、固定 query/fixture、失败注入和本地测量验收。特别是 Websurfx 的 JSON route 是源码可观察契约而非已版本化的独立 API 文档，适配器必须把 rolling commit/version 固定下来。

### Windows 桌面 sidecar 分发：可行，但不是“把 Docker 塞进安装包”

**结论：SearXNG 不是基于 Docker 实现的服务，而是 Python/Flask/HTTPX 应用；Docker/Podman 是官方维护最完整的部署形态。** 对 Stravia Windows 桌面版，若要求用户不预装 Docker、WSL2 或 Python，推荐发行一个**原生 Windows 资源目录 + 受桌面后端监管的子进程**，而不是内嵌 Docker daemon，也不是先假设 PyInstaller `--onefile` 能可靠冻结整个项目。容器路径保留为管理员可选的 external endpoint。

#### 本机实测：原样不能跑，窄 patch 后可以跑

2026-08-17 在当前 Windows 11 x64 工作站对 SearXNG commit [`b2da6b90f`](https://github.com/searxng/searxng/commit/b2da6b90f2f8446557c91f67d6be5064ab785ecd)、CPython 3.13 做了以下一次性 PoC；这些是本次实测结果，不是上游兼容性承诺：

1. `git clone` 的 checkout 因仓库路径 `utils/templates/etc/httpd/sites-available/searxng.conf:socket` 含 Windows 文件名不允许的 `:` 而失败；Windows build 必须消费由 Linux CI 导出的 runtime-only source archive，或在解包时明确排除此类非运行时文件。
2. 固定的 `requirements.txt` 可在 Windows x64 安装对应 wheels，但首次 import 因 `searx/valkeydb.py` 无条件 `import pwd` 失败；即使本地配置不启用 Valkey，也会经过该 import。将仅用于连接失败日志的 Unix account lookup 改为跨平台日志后继续启动。
3. Windows 没有系统 IANA timezone database；默认 engine 装载会因缺少 `tzdata` 使 `bilibili` 失败。Windows artifact 必须额外锁定并携带 `tzdata`，或只加载不依赖它的最小 engine 清单；为避免未来 engine 切换再次踩坑，推荐直接携带。
4. `searx.webutils.get_result_templates` 用 `os.path.join` 生成 `\`，但后续以 `/` 比较模板名，导致 HTML 渲染 `TemplateNotFound`。将内部模板 key 规范化为 `/` 后恢复。
5. 加上上述两个窄 patch、`tzdata`、非默认 `SEARXNG_SECRET` 与 `search.formats: [json]` 后，`GET /healthz` 实际返回 `OK`；`granian==2.8.1` 有 CPython 3.13 `win_amd64` wheel，WSGI 进程实际启动；DuckDuckGo 查询与 `engines=sogou wechat` 的“人工智能”查询均实际返回 JSON 结果。Granian 官方 SearXNG 文档仍注明其目前只在 container installation 中获得官方支持，因此此结果只能作为 Stravia 下游 build 的 PoC 证据（[SearXNG Granian 文档](https://docs.searxng.org/admin/installation-granian.html)、[Granian README](https://github.com/emmett-framework/granian)）。

这组结果改变的是风险判断：**Windows native 不是不可行，而是必须拥有 downstream patch 和打包测试。** 不能把未经 patch 的 upstream checkout、普通 venv 或“开发机上装了 Python”当成可发行 sidecar。

#### 推荐制品：目录型 runtime，不做单文件冻结

推荐制品布局：

```text
Stravia resources/
└── searxng/
    ├── manifest.json                 # commit、Python/wheel/patch hash、bundle schema
    ├── LICENSE                       # SearXNG AGPL
    ├── THIRD_PARTY_NOTICES
    ├── python/
    │   ├── pythonw.exe
    │   ├── python313.dll
    │   ├── python313.zip
    │   ├── python313._pth
    │   ├── Lib/site-packages/        # exact Windows wheels
    │   └── app/searx/                # runtime-only source + version_frozen.py
    └── settings.template.yml
```

CPython 官方 embeddable package 的目的正是把最小 Python runtime 嵌入较大应用：它与用户系统、registry 和已安装 packages 基本隔离，带 `python.exe/pythonw.exe`、DLL 和压缩 stdlib；`._pth` 由 embedder 明确控制搜索路径。它不带 `pip`，官方要求 installer 将第三方 packages 作为应用的一部分一同 vendoring，这正适合固定 SearXNG wheels 的不可变资源目录（[Python Windows embeddable package](https://docs.python.org/3/using/windows.html#the-embeddable-package)）。

不推荐把首版目标设为 PyInstaller `--onefile`：SearXNG 包含大量动态 engine modules、templates、static/data、Babel/timezone 和 compiled wheels；单文件自解压还增加启动与杀毒软件行为变量。Tauri 官方把 PyInstaller 作为 Python sidecar 的常见示例，但这不构成 SearXNG 可冻结证明（[Tauri sidecar](https://v2.tauri.app/develop/sidecar/)）。若以后评估 PyInstaller，只比较 `--onedir` 与上述 embeddable runtime，并要求在干净 Windows VM 做完全相同的 smoke test。

#### 可复现 build pipeline

1. **Linux source stage：** checkout 固定 SearXNG commit；生成 `searx/version_frozen.py`；仅导出运行需要的 `searx/`、license、requirements 与 provenance，排除 Windows 非法文件名、symlink 和部署模板。保存原始 source archive、patch 文件和 SHA-256。
2. **Windows artifact stage：** 固定一个 SearXNG 支持的 CPython minor/patch 与 `win_amd64`；取得官方 embeddable runtime；按 upstream `requirements.txt`、`requirements-server.txt` 加 Stravia-owned `tzdata` lock 下载/解包 Windows wheels；应用上述 compatibility patch；配置 `python313._pth` 只包含 stdlib、vendored site-packages 和 app，不读取用户 `PYTHONPATH`。
3. **最小运行配置：** 使用 `use_default_settings.engines.keep_only` 只保留 Stravia 明确支持并验收过的 engines；`search.formats: [json]`，不开放 HTML UI；`bind_address: 127.0.0.1`；每次启动生成非默认 `server.secret_key`；`limiter: false` 且不配置 `valkey.url`，所以本地单用户 sidecar 不需要另起 Valkey service。`keep_only` 是 SearXNG 官方支持的用户配置合并能力（[settings / `keep_only`](https://docs.searxng.org/admin/settings/settings.html#use-default-settings)）。
4. **Windows smoke/sign stage：** 在没有 system Python、Docker、WSL2、Valkey 的干净 VM 启动 Granian WSGI，等待 `/healthz`；分别请求一个通用 engine 和 `sogou wechat` JSON fixture；检查 `unresponsive_engines`、退出码和日志；关闭 app 后确认无 Python/Granian child；最后对整个目录签名并生成 manifest。

runtime source、Python 与 wheels 必须作为一个原子版本更新，不能让用户在 sidecar 目录中运行 `pip upgrade`。SearXNG 是 rolling release，Stravia 应固定 commit 和 patch-set version，不跟随 `master` 启动时自动更新。

#### Stravia 进程与模块归属

- Tauri `bundle.resources` 可以打包文件或整个目录并保留到 `$RESOURCE`；适合放置上述不可变 runtime（[Tauri config schema `resources`](https://schema.tauri.app/config/2)）。桌面 Rust 后端从 `resource_dir()` 定位 `pythonw.exe`，不要把可执行路径、port 或 secret 暴露给 WebUI。
- 当前 Stravia desktop 已在 `ExitRequested` 调用 `DesktopGatewayRuntime::request_shutdown()`，但没有 SearXNG child owner。新增桌面侧 `SearxngSidecarRuntime`，拥有 `tokio::process::Child`、stdout/stderr drain、readiness deadline、shutdown 和 Windows Job Object；app exit 先请求 Granian 退出，超时后终止整个 Job。不能只 kill 直接 child 而遗留 Granian worker。
- 只有桌面 transport 层拥有 sidecar lifecycle；`stravia-core` 仍只看到一个 loopback `WebProviderAdapter::search` endpoint 与 immutable runtime snapshot。standalone server 不应隐式启动桌面 sidecar。
- Tauri `externalBin` 适合单独 launcher binary，且要求文件名带 target-triple suffix；本方案的主体是 companion directory，因此可直接用 `bundle.resources` + Rust process owner。若后续增加专用 `searxng-sidecar.exe` launcher，再将 launcher 放 `externalBin`，不要为了使用“sidecar”名词强行增加一层空 wrapper（[Tauri sidecar target naming](https://v2.tauri.app/develop/sidecar/)）。

启动顺序：

```text
admin enables local SearXNG provider
  -> desktop verifies manifest/signature and writes per-run settings
  -> select loopback port; spawn pythonw -m granian ... searx.webapp:app
  -> drain logs; wait GET /healthz with a fixed deadline
  -> publish provider endpoint snapshot to WebAccessEngine
  -> search adapter calls GET /search?...&format=json
  -> app exit/provider disable: stop accepting calls, terminate Job, remove run secret
```

Granian `--port 0` 的日志在本次 PoC 中只报告 `:0`，没有给 parent 可消费的实际端口；首版应由 desktop 选择可用 loopback port、立即 spawn，并在 bind failure 时明确报错，不能依赖解析 `:0` 日志发现 endpoint。若要彻底消除“探测端口后释放再 bind”的竞态，应在后续 launcher 中实现 socket inheritance 或由受控 loopback reverse proxy 持有 listener；不要用无界端口重试掩盖。

#### 安全、许可与更新边界

- `server.secret_key` 是 SearXNG cookie/HMAC secret，**不是 HTTP bearer authentication**。仅绑定 `127.0.0.1`、关闭 HTML/CORS surface、只允许 JSON 和 selected engines，可阻止远程网络访问但不能隔离同一用户下的其他本地进程。若 threat model 要求抵御本地 peer process，需要在 desktop 与 SearXNG 之间增加带随机 bearer token 的 loopback reverse proxy，或维护一个最小 auth middleware patch；这应作为显式安全需求，而不是假装 secret 已提供 API auth。
- settings、logs、cache 和临时文件写到 `%LOCALAPPDATA%\Stravia\...`；resource runtime 保持只读。secret 不进数据库、不进 WebUI、不写日志。stdout/stderr 必须持续 drain 并做大小/敏感字段处理，避免 pipe backpressure 或泄露 query。
- bundle manifest 固定 SearXNG commit、CPython、每个 wheel、patch-set、settings schema 和 license hash；升级先写新版本目录、验证后切换，失败保留上一个完整目录，不能原地覆盖一半 runtime。
- SearXNG 为 AGPL-3.0-or-later。发行修改后的 Windows artifact 时必须保留 license/notice 并提供对应 source 与 patch；独立 HTTP 子进程有助于保持工程 seam，但不会自动免除 AGPL 义务。商业发行前需由项目维护者完成 license review；本文不是法律意见（[SearXNG LICENSE](https://raw.githubusercontent.com/searxng/searxng/master/LICENSE)）。

#### Windows 分发选择

| 方案 | 用户前置条件 | Stravia 维护责任 | 微信/engine 覆盖 | 建议 |
|---|---|---|---|---|
| Embeddable CPython + pinned SearXNG directory | 无 Python/Docker/WSL2 | Windows patch、wheel/runtime、签名、进程与 license | 保留 SearXNG engine 生态；本次实测 `sogou wechat` 成功 | **无前置依赖桌面版首选，但必须先完成 artifact PoC** |
| Docker/Podman external endpoint | 已安装并运行 container daemon；Windows 通常还涉及 WSL2/Hyper-V | 只维护 compose/image pin 与 endpoint adapter | 官方最成熟部署路径 | 高级用户/服务器首选，不作为默认桌面 sidecar |
| 用户自建远程 SearXNG | 可达的受信 endpoint | base URL、TLS/auth/allowlist 与 API schema | 由管理员实例决定 | 企业/NAS 最省桌面安装体积 |
| Websurfx native Rust | 需要另做 Windows build/运行验证 | rolling pin、schema/provenance、engine 实现 | 当前无 `sogou wechat` 等 SearXNG 广覆盖 | Rust-only 实验，不替代本次微信需求 |
| 捆绑 Docker daemon 或自动安装 WSL2 | 管理员权限、虚拟化、系统服务 | 巨大的 installer、更新、权限和支持面 | 与官方容器相同 | **拒绝作为 Stravia 默认安装路径** |

上线前最低验收：干净 Windows x64 VM 无 Python/Docker 仍可启动；只监听 `127.0.0.1`；`/healthz`、DuckDuckGo 与 `sogou wechat` JSON 均通过；关闭/崩溃后无 child；损坏/签名不符 artifact 拒绝启动；offline、CAPTCHA/429、单 engine timeout 返回稳定 partial/error；AGPL corresponding source 与 patch 能从发行包说明中取得。安装体积、冷启动、RSS、杀软误报和 engine 稳定率在完成实际 bundle 后测量，当前不写推断值。

### 默认启用 / 零配置 engine 清单（源码可复算）

这里的“零配置”是**配置状态**而不是可用性承诺：只表示该 engine 在上游默认配置中没有 `disabled: true` 或 `inactive: true`，且不要求调用方填写 API key。实际上仍可能遇到 CAPTCHA、429、区域限制、上游维护或网络失败。SearXNG 的 `settings.yml` 还把 `ahmia`、`torch` 配在 `categories: onions`；它们需要 Tor 才有意义，因此从零配置清单排除，但单纯按 `disabled`/`inactive` 解析时仍会得到 active 85。

**SearXNG master 的机器可核对数量：** `engines:` 下 345 条记录；其中 230 条有 `disabled: true`，73 条有 `inactive: true`（两者可重叠）；`not disabled and not inactive` 得 85；再排除需 Tor 的 `ahmia`、`torch`，得到 **83 个零配置候选**。官方 configured-engines 文档当前构建写作 **270 configured / 83 enabled by default**；270 正好对应源码中排除 73 个 inactive 和 2 个 onions/Tor 项后的可见配置数，故不要把 master 文件的 345 条记录或 270 条 configured 全写成零配置。以下名称逐字保留 `settings.yml` 的 `name`：
下列分组仅为便于阅读的用途归类，不等同于 SearXNG `categories` 原始字段；engine 名称和总数以 `settings.yml` 为准。

- **通用网页 / 站点 / 社区（12）：** `artic`、`chefkoch`、`duckduckgo`、`lemmy communities`、`lemmy users`、`lemmy posts`、`lemmy comments`、`mastodon users`、`mastodon hashtags`、`startpage`、`google cse`、`brave`。
- **媒体（33）：** `bandcamp`、`bing images`、`bing news`、`bing videos`、`openverse`、`deviantart`、`duckduckgo images`、`duckduckgo videos`、`duckduckgo news`、`flickr`、`genius`、`google news`、`google cse images`、`mixcloud`、`pexels`、`pinterest`、`radio browser`、`reuters`、`sepiasearch`、`soundcloud`、`startpage news`、`startpage images`、`unsplash`、`youtube`、`dailymotion`、`vimeo`、`wikinews`、`wikicommons.images`、`wikicommons.videos`、`wikicommons.audio`、`brave.images`、`brave.videos`、`brave.news`。
- **知识 / 科学 / 代码（24）：** `arch linux wiki`、`arxiv`、`wikipedia`、`wikidata`、`devicons`、`docker hub`、`github`、`google scholar`、`gentoo`、`etymonline`、`hoogle`、`mdn`、`mankier`、`openairedatasets`、`openairepublications`、`pdbe`、`pubmed`、`pypi`、`stackoverflow`、`askubuntu`、`superuser`、`semantic scholar`、`wiktionary`、`wordnik`。
- **utility / 地图 / 翻译（9）：** `currency`、`lingva`、`lucide`、`openstreetmap`、`photon`、`dictzone`、`mymemory translated`、`tootfinder`、`wttr.in`。
- **文件 / torrent（5）：** `kickass`、`piratebay`、`solidtorrents`、`wikicommons.files`、`bt4g`。

上述 12+33+24+9+5 = **83**。同一配置中 `ahmia`、`torch` 是“settings active 但需 Tor”的 2 项，不属于零配置；它们不是隐藏的公网 web engine。需要 key/token 或显式凭据的 engine 也不属于零配置；master 中可由字段/注释核对的名单是：`astrophysics data system`、`cloudflareai`、`core.ac.uk`、`flickr_api`、`freesound`、`jina`、`libretranslate`、`marginalia`、`springer nature`、`Torznab EZTV`、`yandex api`、`youtube_api`、`wolframalpha_api`、`deepl`、`wallhaven`、`braveapi`、`exaapi`。它们通常同时是 `inactive: true`；其余未列出的默认禁用/非活动项仍不应从“配置存在”推断为零配置。按原始字段计，默认禁用项为 230 条、非活动项为 73 条；这两个计数不是互斥集合。

**Websurfx rolling 的机器可核对数量：** `src/engines/mod.rs` 与 `EngineHandler::new` 共同注册 11 项：`duckduckgo`、`searx`、`brave`、`startpage`、`librex`、`mojeek`、`bing`、`wikipedia`、`yahoo`、`qwant`、`sepiasearch`。`websurfx/config.lua` 的 `upstream_search_engines` 默认值只有 **2/11 为 true**：`DuckDuckGo`、`Wikipedia`；另外 9 项 `Searx`、`Brave`、`Startpage`、`LibreX`、`Mojeek`、`Bing`、`Qwant`、`Yahoo`、`SepiaSearch` 明确为 `false`，只是已编译注册、可配置，不是默认启用。这里保留 config 的大小写原名；registry 的规范化名称见上一个列表。

两项目录都只表达默认配置和注册状态；即使列为零配置/default-enabled，也不保证每次实时请求成功，不能据此承诺上游可达性、无 CAPTCHA/429 或无区域限制。
**接口提醒：** SearXNG 默认 `search.formats` 只有 `html`；Stravia 要调用 JSON Search API，仍需管理员显式启用 `json`。因此“engine 零配置”不等于“Stravia adapter 零配置”（[`settings.yml`](https://raw.githubusercontent.com/searxng/searxng/master/searx/settings.yml)、[Search API](https://docs.searxng.org/dev/search_api.html)）。

### 排除混淆项

- `Tantivy`、`Meilisearch`、Lucene/Solr/OpenSearch 等在已有 documents 上提供索引/查询能力。Tantivy 官方明确称自己不是现成 server；Meilisearch 官方 getting started 前提是先 add documents to an index（[Tantivy](https://github.com/quickwit-oss/tantivy)、[Meilisearch](https://github.com/meilisearch/meilisearch)）。没有 crawler、fetch、robots、正文抽取和 freshness pipeline 时，不应写成“完全本地 web search”。
- SearXNG 的本地进程与本地索引不是同义词；关闭上游后不能保证公网召回（[Search API](https://docs.searxng.org/dev/search_api.html)）。
- YaCy 的 P2P default operation 不是 local-only；PoC 必须退出 cluster/network 并只查询 seeded local index（[YaCy README](https://github.com/yacy/yacy_search_server)）。
- `robots.txt` 不是访问授权；登录、付费墙、版权/ToS 和法律义务不能由 parser 解决（[RFC 9309 §1](https://www.rfc-editor.org/rfc/rfc9309#section-1)）。

## 可直接替代 leaf tools 的现成项目与排除项

### 只评估 `search` / `fetch` provider 能力

本节只回答项目能否直接承担 `WebProviderAdapter::search`、`WebProviderAdapter::fetch`，或能否通过稳定本地协议适配这两个方法。A 是搜索后端/元搜索（返回 URL、标题、snippet）；B 是 fetch/crawl/正文抽取（给定 URL 或站点）。带 MCP、Ollama、本地部署或 AI-search UI 不构成入选条件；MCP 的 streamable HTTP 也不等于 REST。

| 候选 | `web_search` | `web_fetch` | 形态、license 与维护信号 | 与 Stravia 的判断 |
|---|---:|---:|---|---|
| native Rust：`reqwest` + `scraper` + `dom_smoothie` + chromiumoxide fallback | 否 | **是** | in-process；MIT/Apache/ISC 组合；动态阶段控制外部 Chromium/CDP；无 DB/queue，但等待、正文/Markdown、安全边界由 Stravia 自己实现 | **最轻 Rust 集成首选**；SearXNG/Websurfx 负责 search，native fast path 只在需要时进入 browser |
| [`SearXNG`](https://github.com/searxng/searxng) | **是** | 否 | Python sidecar，AGPL-3.0；官方 Docker/Podman 与 JSON API | **search 生产首选**；聚合公网 upstream，不是自有 index |
| [`Scrapling`](https://github.com/D4Vinci/Scrapling) MCP | 否 | **是，含 Playwright JS** | Python/BSD-3-Clause；MCP stdio 或 MCP streamable HTTP；Response 可 Markdown/HTML/text；没有通用 search tool；官方 Docker + Chromium | **最轻现成进程**；配 SearXNG；不要称其 transport 为 REST |
| [`Crawl4AI`](https://github.com/unclecode/crawl4ai) REST | 否（GoogleSearchCrawler 只抓 Google SERP，不是通用 search/index） | **是，含 Playwright JS** | Python/Apache-2.0；FastAPI `/crawl`/`/crawl/stream`，Markdown/结构化抽取；Dockerfile 带 Playwright、Redis、supervisor，官方部署至少 4GB RAM | **最轻现成 REST**；配 SearXNG；4GB 是官方部署前提，不是比 Firecrawl 轻的资源 benchmark |
| [`Lightpanda`](https://github.com/lightpanda-io/browser) | 否 | **是，CLI/CDP + Markdown dump** | Zig/AGPL-3.0；独立 browser，不是 Chromium；Beta/WIP；Docker 只列 Linux amd64/arm64，Windows 需 WSL2 | **仅实验**；不纳入 Windows production short-list |
| [`open-webSearch`](https://github.com/Aas-ee/open-webSearch) | **是** | **是，但不保证 JS-heavy** | TypeScript、Apache-2.0；local daemon REST/MCP；README 明确部分 JS-heavy landing page 可能没有可读正文 | 轻量双 leaf PoC；不满足“必须支持 JS-heavy”保证 |
| [`Websurfx`](https://github.com/neon-mmd/websurfx) | **是** | 否 | Rust、AGPL-3.0；Cargo + optional Redis/memory cache、Docker；源码实际 JSON route `?q=...&json=true`，README 却示例 `format=json=true`（[rolling README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)、[route](https://github.com/neon-mmd/websurfx/blob/rolling/src/routes/search.rs)、[SearchParams](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/search_route.rs)） | **Rust-only/实验对照**；不能替代 fetch；API/provenance 必须 PoC 锁定 |
| [`Firecrawl`](https://github.com/firecrawl/firecrawl) | **是；上游 search route** | **是，含 Playwright/JS** | TypeScript、主仓库 AGPL-3.0；self-host 官方 stack 有 `/v2/search`、`/v2/scrape` 与 Playwright processing；Docker Compose baseline 无 durable storage/auth/TLS/HA | **仍是成熟单服务双 leaf 基准**；search 不是自有 web index，且整体运维边界比上述 fetch-only 选择宽 |
| [`Browserless`](https://github.com/browserless/browserless) | **仅 Cloud Search API** | 是（self-host CDP；REST routes 按产品/部署） | TypeScript/Docker；license 为 SSPL-1.0 OR Browserless Commercial License；官方 Search API 明确 Cloud plans only | API/license 清楚但不符合本地开源 search 要求；不纳入轻量推荐 |
| [`xynehq/websearch`](https://github.com/xynehq/websearch) | **是** | 否 | Rust、MIT；README/manifest 证据仍显示小型 library/CLI | 不推荐；重复现有 provider adapters，不能替代 fetch |

### leaf tool 适配结论

- **三档收敛：** (1) 最轻 Rust 集成 = native `reqwest` fast path + chromiumoxide/外部 Chromium fallback；(2) 最窄现成 service = Scrapling MCP + browser child/session，配 SearXNG；(3) 最窄现成 REST = Crawl4AI 单容器 FastAPI `/crawl`，配 SearXNG。
- **Firecrawl 仍是 JS-heavy 双 leaf 的成熟单服务基准。** 它适合验证同一 sidecar 的 search/scrape 映射；但官方事实没有证明它拥有 web index，search route 不能被写成“本地自有搜索”。
- **不存在明确更轻的成熟单服务双 leaf 替代。** Rust libraries 没有 search/service；Scrapling 与 Crawl4AI 没有通用 search；Lightpanda 是 Beta/WIP 且只实验；Browserless 的 Search API 是 Cloud-only。若必须同时满足本地 `search + JS-heavy fetch`，不能用“更轻”措辞替换 Firecrawl，应该拆成 SearXNG + fetch backend。
- **只做 fetch 时才有可核验的轻量替代。** Rust 集成不带 DB/queue；Scrapling MCP 没有可证实的 DB/queue 且可用 stdio/streamable HTTP，但仍启动浏览器 child；Crawl4AI REST 是更窄的 fetch-only API，却包含浏览器及 Docker 内 Redis/supervisor，官方至少 4GB 不能当作资源更轻证明。
- **排除 runner/UI：**Vane、Morphic、GPT Researcher、MindSearch、OpenDeepSearch、Local Deep Researcher 不作为 `web_search`/`web_fetch` provider；它们会重复 `WebResearchRunner` 已有职责。

### 最终短名单

| 目标 | 首选 | 次选 | 不选 / 约束 |
|---|---|---|---|
| 最轻 Rust 集成（fetch） | **native reqwest fast path + chromiumoxide/外部 Chromium fallback** | headless_chrome fallback | 需配 SearXNG/Websurfx；自实现等待、正文/Markdown、安全与 browser egress；不带 search |
| 最窄现成 service（JS-heavy fetch） | **Scrapling MCP + Playwright/Chromium child/session** | Lightpanda（仅实验） | 配 SearXNG；Scrapling 不是 REST；Lightpanda Beta/WIP、AGPL、Windows 需 WSL2 |
| 最轻现成 REST（JS-heavy fetch） | **Crawl4AI `/crawl`** | Firecrawl `/v2/scrape` | 配 SearXNG；Crawl4AI 官方至少 4GB，Docker 含 Redis/supervisor，不作资源 benchmark |
| 一个服务替代两个 tools，且支持 JS-heavy | **Firecrawl**（成熟基准） | 无明确更轻的成熟本地开源单体 | open-webSearch 不保证 JS；Browserless Search Cloud-only；其他候选无通用 search |
| 只替代 `web_search` | **SearXNG（生产默认）** | **Websurfx（Rust-only/实验）** | 两者都是依赖公网 upstream 的 metasearch、都不是自有 index；Websurfx JSON README/source 契约矛盾，需 PoC；Tantivy 单独、xynehq/websearch 不是完整 web search service |
| 只替代静态 `web_fetch` | **native Rust：reqwest + scraper + dom_smoothie** | Scrapling static Fetcher | Spider crawler；浏览器不是默认路径 |
| 断网/自有 corpus search | **YaCy local-only** | Spider + Tantivy | SearXNG/Websurfx；它们依赖 upstream，不是自有 index |

## 国内社交媒体覆盖（微信公众号、小红书及主要平台）

### 先给直接答案：官方搜索 API 与可用的本地方案不是一回事

截至本次核验，在已核验的微信公众号官方文档中**未找到“按关键词搜索全平台公开公众号文章”接口**。微信服务号文档列出的草稿箱接口是当前账号草稿的增删改查，素材接口是按 `media_id` 或当前公众号素材列表管理，发布能力是获取/删除当前账号已发布消息；这些是账号自有内容管理，不是全网发现（[草稿箱](https://developers.weixin.qq.com/doc/service/guide/product/draft.html)、[素材管理](https://developers.weixin.qq.com/doc/service/guide/product/asset.html)、[发布能力](https://developers.weixin.qq.com/doc/service/guide/product/publish.html)）。因此“搜索公众号全网文章”不能被表述成微信官方开放 API。未知点仍应保留为未知：官方文档未找到不等于证明平台内部绝对不存在其他受限能力。

在已核验的小红书官方开放平台公开入口/文档中也**未找到“按关键词搜索全站公开笔记”接口**。公开入口是小红书开放平台（当前页面可见能力以电商开放平台的商品、订单、售后、财务等为主），不能把网页/App 内部搜索请求、`search.xiaohongshu.com` 或第三方文章声称的路径当作开放平台 API（[官方开放平台入口](https://open.xiaohongshu.com/)、[官方文档入口](https://open.xiaohongshu.com/document/developer/file/53)）。这同样是“在已核验官方文档中未找到”，不是对私有/登录态接口作绝对否定。

故可执行的选择只有三类：

1. **官方账号自有内容 API**：只同步管理员有权访问的公众号草稿/素材/发布记录；不能扩展成全平台搜索。
2. **公共垂直搜索/网页索引尝试**：微信公众号优先用 SearXNG 已内置但默认关闭的 `sogou wechat` engine；其他平台再用 SearXNG/Websurfx 或直接搜索引擎的 `site:` 查询。它们都依赖上游当时的公开索引，不能承诺全量、实时、稳定分页或稳定可达。
3. **受控登录态采集器或 bounded archive**：MediaCrawler、xiaohongshu-mcp 等是浏览器/网页采集工具，不是稳定官方搜索 API；仅允许管理员人工提供账号、明确范围、低频 PoC，或对已有文章 URL/账号自有导出做归档。

### 平台矩阵

| 平台 | 已核验官方公开能力 | 公共索引尝试 | 开源候选/登录要求 | Stravia 结论 |
|---|---|---|---|---|
| 微信公众号 | 草稿、素材、发布记录等**当前公众号自有内容**；未找到全平台公开文章关键词 API | SearXNG `sogou wechat`（默认关闭、无需 API key、抓取搜狗微信 HTML）可做关键词发现；`site:mp.weixin.qq.com` 只作补充 | `weixin-articles-mcp`（MIT，给定公开文章 URL→Markdown/图片/视频元数据）；`wechatmp2markdown`（Go/MIT，给定 URL→Markdown/ZIP）；二者不做全网发现、无需 Cookie | **发现 PoC 首选 SearXNG `sogou wechat`，正文归档首选已知 URL fetch**；所有 search 结果标 `remote/indexed/partial` |
| 小红书 | 在已核验官方开放平台文档中未找到全站公开笔记关键词搜索 API | `site:xiaohongshu.com` 只是一种索引覆盖尝试 | `xpzouying/xiaohongshu-mcp`（Go/Apache-2.0，README 要求首次登录，`search_feeds` 走浏览器页面）；MediaCrawler（Python，非商业学习许可证，需保存登录态） | 可做独立、人工登录的 search-only PoC；禁止将发布、点赞、评论、Cookie 等工具暴露给模型 |
| 微博 | 本次未核验到可供 Stravia 使用的全平台公开内容关键词官方 API | `site:weibo.com` 仅索引尝试 | MediaCrawler README 列出关键词搜索/指定帖子/主页，但属于登录态采集器；官方 API/权限范围未知 | 不推荐写入生产覆盖承诺；需单独官方授权或 bounded corpus |
| 抖音 | 本次未核验到可供 Stravia 使用的全平台公开内容关键词官方 API | `site:douyin.com` 仅索引尝试 | MediaCrawler README 列出关键词搜索/详情/主页，需登录态；不是官方 API | 仅受控 PoC；不承诺覆盖和稳定性 |
| B 站 | 本次未核验到可供 Stravia 使用的全平台公开内容关键词官方 API | `site:bilibili.com` 仅索引尝试 | MediaCrawler README 列出关键词搜索/详情/主页；登录、签名和站点限制仍由项目处理 | 只可当公网索引或受控采集实验 |
| 知乎 | 本次未核验到可供 Stravia 使用的全平台公开内容关键词官方 API | `site:zhihu.com` 仅索引尝试 | MediaCrawler README 列出问答/文章等抓取能力；不等于官方搜索 API | 仅 bounded PoC |

矩阵中“未核验到”都必须保留为未知；不能从不存在公开文档推导“绝对不存在”。任何第三方商业数据 API 也必须单独标为商业服务（合同、授权、数据范围和保留期限另行核对），不能冒充官方接口或开源 crawler。

### SearXNG 当前中国 engine 状态与 `site:` 边界

SearXNG master 的官方 `settings.yml` 当前把以下 engine 明确标为 `disabled: true`：

| engine | 当前配置 | 解释 |
|---|---|---|
| `baidu`、`baidu images`、`baidu kaifa` | disabled | 已注册但默认不运行；不能写成默认百度覆盖 |
| `360search`、`360search videos` | disabled | 已注册但默认不运行 |
| `sogou`、`sogou images`、`sogou videos`、`sogou wechat` | disabled | 已注册但默认不运行 |
| `bilibili` | disabled | 已注册但默认不运行；仅覆盖 B 站，不覆盖其他社交平台 |

官方源码中的 `sogou_wechat.py` 进一步写明 `use_official_api: False`、`require_api_key: False`、结果类型为 HTML，并构造 `https://weixin.sogou.com/weixin?type=2&query=...&page=...`；这是一段抓取上游 HTML 的 engine 实现，不是搜狗官方 API（[settings.yml](https://raw.githubusercontent.com/searxng/searxng/master/searx/settings.yml)、[sogou_wechat.py](https://raw.githubusercontent.com/searxng/searxng/master/searx/engines/sogou_wechat.py)）。一次性人工 GET `https://weixin.sogou.com/weixin?type=2&query=人工智能&page=1` 曾返回标题为“人工智能的相关微信公众号文章 – 搜狗微信搜索”的页面并展示结果计数；这只是本次 PoC 的观测，不是 SLA、稳定分页或全量保证。
因此微信公众号的最小 PoC 不需要再部署一个专用搜索脚本：在受控 SearXNG 实例中覆盖 `sogou wechat` 为 `disabled: false`，同时按前文开启 JSON Search API，然后限定 `engines=sogou wechat` 查询即可。这样复用现有 SearXNG adapter、超时、错误与 provenance 边界；仍必须标成 `remote/indexed/partial`，不能标成官方 API 或本地自有索引。

启用这些 disabled engine 也不会让 SearXNG 变成中国社交媒体的完整索引：上游可能返回 CAPTCHA、429、区域限制、空结果或变更 HTML；engine 的注册/配置状态不是可用性承诺。`site:mp.weixin.qq.com`、`site:xiaohongshu.com`（以及微博、抖音、B 站、知乎对应域名）只能测试公共搜索引擎**当前是否收录了部分页面**，不能证明平台全量、最新、可搜索，也不能发现未被索引或需登录的内容。SearXNG 与 Websurfx 都是元搜索，依赖配置的上游；**二者默认不能完整覆盖中国社交媒体**，不能宣传为本地完整搜索或自有 index。

### 候选逐项核验：search、crawl、export、账号自有内容

| 候选 | README / 源码入口核验 | 登录/Cookie | 输出与真实边界 | license / 维护信号 | 推荐层级 |
|---|---|---|---|---|---|
| [`xpzouying/xiaohongshu-mcp`](https://github.com/xpzouying/xiaohongshu-mcp) | Go MCP server；README 列出 `search_feeds`（keyword + `sort_by`/`note_type`/`publish_time`/`search_scope`/`location`），默认 Streamable HTTP `http://localhost:18060/mcp`；[`xiaohongshu/search.go`](https://raw.githubusercontent.com/xpzouying/xiaohongshu-mcp/main/xiaohongshu/search.go) 通过 Rod 导航搜索页并读取 `window.__INITIAL_STATE__.search.feeds` | README 明确首次必须登录；凭据/浏览器状态由管理员隔离，绝不送入模型 | **网页登录态 search/crawl**；同仓库还暴露发布、评论、点赞、收藏、删除 Cookie 等写工具，不能把全 MCP 面暴露给 Stravia | Apache-2.0；GitHub 有 Windows/Linux/macOS releases、Docker 和持续贡献信号；不承诺站点兼容性/SLA | **小红书 search-only PoC 首选**；专用 sidecar + allowlist |
| [`NanmiCoder/MediaCrawler`](https://github.com/NanmiCoder/MediaCrawler) | README 的平台矩阵列出小红书、抖音、快手、B 站、微博、贴吧、知乎的关键词搜索、帖子详情、评论、创作者主页；入口 `main.py --platform xhs --lt qrcode --type search`；支持 CSV/JSON/JSONL/Excel/SQLite/MySQL 导出 | Playwright/CDP 保存登录态、Cookie；不能把用户凭据交给模型 | **多平台 crawl/search/export**，不支持微信公众号；不是官方 API，也不提供全平台 index | LICENSE 是 Non-Commercial Learning License 1.1，禁止商业用途及大规模抓取；GitHub 当前非 archived、近期有 push，维护信号强但许可证不适合 Stravia 商业生产 | **研究验证**；商业生产排除 |
| [`wechat-article/wechat-article-exporter`](https://github.com/wechat-article/wechat-article-exporter) | TypeScript WebUI/worker；README 历史上支持关键词搜公众号、历史文章批量导出；但 README 明确项目于 2026-07-30 停止维护、依赖的微信上游核心接口已关闭并转只读归档 | 需要后台扫码/credentials 的历史能力；不应再尝试绕过已关闭接口 | 曾是**发现 + crawl + export**，现在只能作为源码/已同步数据参考，不是可用 API | MIT；README 明确不再接受 issue/PR、无新版本 | **排除；不纳入短名单** |
| [`jj-cheng25/weixin-articles-mcp`](https://github.com/jj-cheng25/weixin-articles-mcp) | Python MCP，入口 [`server.py`](https://raw.githubusercontent.com/jj-cheng25/weixin-articles-mcp/main/src/weixin_articles_mcp/server.py) 只注册 `read_article(url)`；README 说明 `httpx` 读取公开 `mp.weixin.qq.com/s/...`、输出 Markdown/图片/视频关键帧，路线图中的 full-text search 仍未完成 | **不使用 Cookie/login**；默认请求间隔 1 秒，声明不绕过 anti-bot | **给定 URL 的 fetch/export/archive**；不是公众号发现、历史列表或关键词 search。适合作为账号自有导出/人工收集 URL 的 bounded corpus 输入 | MIT；官方 GitHub API 标记非 archived，2026-04 创建、2026-05 push；规模小，不能据此承诺长期维护 | **微信公众号账号自有/已授权 URL archive 首选** |
| [`fengxxc/wechatmp2markdown`](https://github.com/fengxxc/wechatmp2markdown) | Go `main.go` 接收单篇 URL + 输出路径，或启动 `server [port]`；README 支持 Windows/Linux/macOS 构建、图片 URL/save/base64、ZIP | 不需要登录态的已知公开 URL fetch；不得扩展成批量发现 | **单篇 export**，不是 search/crawl；适合已有 URL 列表的离线 Markdown 归档 | MIT；仓库有明确 LICENSE；长期维护信号未在本次核验中确认 | **账号自有/已授权 URL 的备选导出器** |

因此，当前短名单应写成：微信公众号关键词发现首选 SearXNG `sogou wechat`（无 API key、非官方 HTML engine、best-effort），已拥有公众号 URL 的正文归档选 `weixin-articles-mcp` 或 `wechatmp2markdown`；小红书 search PoC 选 `xiaohongshu-mcp`（登录态、专用 search-only sidecar）；微博/抖音/B 站/知乎等多平台研究可选 MediaCrawler，但其非商业许可证将其排除在 Stravia 商业生产外。没有足够一手证据把任何 crawler 称为稳定官方搜索 API，也没有核验到一个无登录且能保证全量公众号覆盖的接口。

### WebProviderAdapter 接入边界、凭据隔离与合规

- `search` adapter 只暴露 `query/max_results/domain filters` 和规范化结果；必须保留 `source=upstream/indexed|browser-session|owned-corpus`、engine、observed_at、partial/error。不得把 crawler 的评论、点赞、发布、删除、任意脚本、代理或 Cookie 参数传播到模型。
- XHS MCP 只能通过专用 loopback/受控 sidecar 的 **search-only allowlist** 接入；`supports_search() -> true`、`supports_fetch()` 需另行实现并限制到公开 URL。启动时关闭或隔离所有写工具，MCP 鉴权 token 仅在服务端配置。
- MediaCrawler 必须是管理员启动的研究 worker；账号、二维码、Cookie、CDP profile、代理凭据保存在独立 secret/profile store，模型只收到脱敏结果。不能要求用户把 Cookie 粘贴到 prompt，也不能把凭据放进 tool result、日志、fixture 或环境回显。
- 任何需要登录/验证码/签名/风控挑战的步骤都由人工在平台支持的客户端完成；PoC 遇到验证码、风控、登录失效、robots/ToS 拒绝、429 或权限不足时**停止并报告**，不重试绕过、不降低检测、不代理轮换规避。
- 账号自有内容同步必须先得到账号所有者授权并限定 URL/时间窗/删除策略；文章版权、个人信息、评论和图片不能因为代码 license 而自动获得再发布权。官方 API、商业数据服务、公共网页索引、登录态采集器和自有 bounded corpus 在报告中分栏。

### 可执行但不规避风控的 PoC query/fixture 与拒绝条件

1. **公共索引 query**：在 SearXNG 开启 JSON API 后，微信公众号先限定 `engines=sogou wechat` 查询 `人工智能`；其他平台分别查询 `site:xiaohongshu.com "人工智能"`、`site:weibo.com "人工智能"`、`site:douyin.com "人工智能"`、`site:bilibili.com "人工智能"`、`site:zhihu.com "人工智能"`。记录 query、engine、结果 URL、observed_at、`unresponsive_engines`，并断言报告使用“上游索引/部分结果”，不是全量。
2. **Sogou WeChat 单次 fixture**：通过受控 SearXNG `sogou wechat` engine 查询；必要时仅用公开的 `https://weixin.sogou.com/weixin?type=2&query=人工智能&page=1` 对照字段，保存脱敏的标题/摘要/公众号/中间链接 fixture。若出现验证码、429、登录或 ToS 提示，立即停止，不调用私有接口，不扩大分页。
3. **XHS search-only fixture**：管理员在隔离浏览器中人工完成登录后，以低频查询 `人工智能`，固定 `sort_by=最新`、`note_type=图文`、`publish_time=一周内`，只读取 `search_feeds` 的标题、笔记 ID、公开 URL、时间和结果页；不得调用 publish/comment/like/favorite/delete-cookie，且不保存 Cookie。验证 MCP allowlist 拒绝其他工具。
4. **账号自有 archive fixture**：由所有者提供 1–20 个 `https://mp.weixin.qq.com/s/...` URL，使用 `read_article` 或 `wechatmp2markdown` 输出 Markdown + 元数据；使用本地 fixture 断言 URL host、正文截断、媒体数量、删除请求和失败状态，不自行发现更多文章。
5. **统一拒绝条件**：结果要求“全平台/全量/实时/稳定排序”、要求交付登录 Cookie/签名/验证码、要求绕过 robots/ToS/访问控制、要求从 crawler 推导官方 API、上游返回 CAPTCHA/429/403、或候选 license 不允许商业使用时，PoC 失败并停止；只能降级为“未知/未覆盖/需官方或商业授权”。

## 安全、合规与运维

### SSRF、DNS 与 redirect

- 入口只接受公网 HTTP(S)，redirect 链逐跳重做 URL、DNS、IP、port、scheme 和 domain policy；禁止 loopback、link-local、RFC1918、ULA、特殊 IPv6、metadata endpoint、`.local`/`home.arpa`。
- 不能采用“首次 DNS lookup 通过，之后让普通 client 自行解析”；必须固定经过检查的地址或使用受控 resolver，并验证 TLS hostname/SNI 与原始 host 一致。PoC 用可控 DNS/redirect fixture 证明 rebinding 和 public→private 链被拒绝。
- 明确 proxy 是 egress boundary 还是绕过 SSRF 的旁路。reqwest 可能读取环境 proxy；管理员需决定 `no_proxy` 及 proxy 解析后如何执行目的地 policy（[README](https://github.com/seanmonstar/reqwest)、[ClientBuilder](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)）。

### 内容、资源和进程隔离

- 分别限制 header、压缩前/后 body、单 URL bytes、总 bytes、解析元素数、输出字符数、redirect hops、DNS/connect/read/total deadline；拒绝危险 MIME，防 decompression bomb 和 parser 消耗失控。
- HTML、Markdown、JSON-LD、标题和 snippet 都是不可信输入；不能把页面文字当 system instruction，也不能把页面 URL/header/cookie/script 权限提升。
- Browser fallback 使用独立 profile、禁用不必要下载/文件访问、低权限用户和 sandbox；隔离 browser crash、内存、CPU、网络和生命周期。不能把 API key、环境变量或用户 session 传给页面。
- robots 按 RFC 9309 记录 User-Agent、获取时间、缓存、失败语义和 rate limit；robots 不替代授权、合同和版权判断。

### 全网规模：为何近期不现实

Common Crawl 官方报告称 2026-06 crawl archive 有 **2.10B pages、40.8M hosts、33.6M registered domains、354.59 TiB uncompressed，WARC compressed 82.68 TiB**（[官方公告](https://commoncrawl.org/blog/june-2026-crawl-archive-now-available)，访问 2026-08-17）。这只是可供下载的一个 archive，不等同于完整、实时、无版权限制的互联网；但它给出可核验的数量级。

另一个透明的**数量级估算（不是 benchmark）**：若目标 10 亿页面，10 pages/s 即 `100,000,000` 秒、约 1,157 天（3.17 年）；100 pages/s 仍约 116 天，不含失败、重试、robots/rate delay、动态渲染、去重和 recrawl。平均 50 KB 原始响应即约 50 TB 十进制，尚未计正文、倒排词典、日志、快照和副本。故完全本地全网 crawler+index 不是选择一个 crate 的问题，而是持续抓取、存储、去重、排名、更新、合规和运维系统工程。近期应承诺有限、可测、可刷新 corpus。

## 对比矩阵

| 方案 | 执行形态 | `fetch` 网络/API | 自有 index | crawler | JS | 正文抽取 | Windows / 容器证据 | license | 近期判断 |
|---|---|---|---:|---:|---:|---:|---|---|---|
| reqwest + url + scraper + dom_smoothie | Rust in-process | 目标 URL（HTTP client） | 否 | 否 | 否（旁路） | 是（heuristic） | reqwest README 有 Windows TLS；组合需 CI | MIT/Apache/ISC（逐项锁定） | **native fast path 现在做** |
| + chromiumoxide / headless_chrome | Rust + 外部 Chrome child/CDP | 目标 URL（无 provider REST） | 否 | 否 | **是** | 返回 HTML/text，再由 Stravia 抽取 | headless_chrome 明列 Linux/Mac/Windows；chromiumoxide fetcher 列 Win32/Win64；Chrome sandbox/image 需验证 | MIT 或 MIT/Apache | **按需 fallback；无 DB/queue** |
| Scrapling MCP | Python MCP process + Playwright/Chromium | MCP stdio 或 streamable HTTP；**非 REST** | 否 | 可选 spider | **是** | MCP `ResponseModel` Markdown/HTML/text | OS Independent classifier；官方 Docker + Chromium；native Windows 细节需 CI | BSD-3-Clause | **最轻现成进程**；无 search，配 SearXNG |
| Crawl4AI `/crawl` | Python FastAPI + Playwright/Chromium；Docker 还含 Redis/supervisor | **REST** `/crawl`、`/crawl/stream` | 否 | **是** | **是** | Markdown/HTML/结构化抽取 | 官方 Docker guide 至少 4GB、Linux multi-arch；native Windows matrix 未确认 | Apache-2.0 | **最轻现成 REST**；无通用 search，配 SearXNG |
| Lightpanda | Zig standalone browser / CDP server | CLI `fetch` 或 CDP；无通用 fetch REST | 否 | 否 | **是** | CLI `--dump markdown` | 官方 Docker Linux amd64/arm64；无 native Windows，需 WSL2 | AGPL-3.0 | **Beta/WIP，仅实验** |
| Spider | Rust crawler | 目标 URL；可选 cloud API | 否（需另接 index） | **是** | 可选 Chrome | export/抽取需验 | 完整 Windows/容器 matrix 未确认 | MIT | **后续 crawl** |
| SearXNG | Python service | search engine API（上游） | 否 | 否 | 非核心 | 非核心 | Docker/Podman 官方；Windows downstream patch PoC 已运行，发行 artifact 仍待验收 | AGPL-3.0-or-later | **公网 search；生产默认** |
| YaCy | Java app | search API / P2P；local-only 可关 | **是** | **是** | 需场景验 | 非独立 API | Windows installer、Docker 官方 | GPL-2.0-or-later，部分 LGPL | **内网/受控 corpus** |
| Stract | Rust distributed | own crawler/index | **是** | **是** | 需验 | 需验 | Linux setup；Windows/container 未确认 | AGPL-3.0 | **仅参考（archived）** |
| Nutch + index server | Java crawler + external index | 目标站点公网 | 需组合 | **是** | plugin | parse plugin | Docker 官方；Windows 未确认 | Apache-2.0 | **pipeline 参考** |
| Tantivy / Meilisearch 单独 | index library/server | 无 web fetch/search engine | 只对导入 docs | 否 | 否 | 否 | Tantivy README 有 Windows；部署各自验证 | Tantivy MIT；Meilisearch CE MIT/EE 另有 license | **不可单独称 web search** |
| open-webSearch | TypeScript daemon/MCP/CLI | search/fetch REST | 否 | 否 | **fetch 不保证** | 是（静态 Readability/站点 fetch） | npm/Windows/Docker 官方 README | Apache-2.0 | **轻量双 leaf PoC** |
| Websurfx | Rust metasearch | search API（上游；源码 route） | 否 | 否 | 非核心 | 否 | Cargo/Redis、Docker；x86_64，Windows 矩阵未承诺 | AGPL-3.0 | **`web_search`-only Rust-only/实验；JSON/provenance 需 PoC** |
| Firecrawl | TypeScript fetch/crawl API | `/v2/search` + `/v2/scrape`（search 非自有 index） | 否 | **是** | **Playwright/JS** | **是；Markdown/JSON** | Docker Compose API :3002；self-host baseline 无 durable storage | AGPL-3.0（SDK/部分 UI 例外） | **成熟双 leaf 基准** |
| Browserless | TypeScript browser service | CDP；Search REST **Cloud-only** | 否 | 可选 | **是** | route-specific | Docker/ghcr 官方；native Windows 未确认 | SSPL-1.0 OR Commercial | **不纳入本地开源 shortlist** |
| xynehq/websearch | Rust library + CLI | search providers | 否 | 否 | 否 | provider-specific | Windows build tools/Dockerfile；未证实 crates.io 发布 | MIT | **不推荐：重复 adapter** |


## 分阶段推荐

### Phase 0：冻结两个 leaf 的映射和安全政策

保持公开 `SearchRequest`/`SearchResponse`、`FetchRequest`/`FetchResponse` 和 `WebProviderAdapter`。写出 query/filter/result normalization，以及 redirect、DNS pinning、proxy/no_proxy、robots、body/CPU/redirect limits；建立静态/动态和 SSRF fixture。runner、prompt、研究轮次和报告格式不在本次变更范围。

### Phase 1：三档 fetch PoC（先验证最小边界）

- **Rust 集成档：** `reqwest` native fast path；只对 pending JS URL 启动/连接 chromiumoxide 或外部 Chromium。必须把等待、正文/Markdown 抽取、浏览器 profile、CDP lifecycle 和 egress policy 写成 adapter 内部边界；不引入 DB/queue。
- **现成进程档：** Scrapling MCP。`supports_fetch() -> true`、`supports_search() -> false`；stdio 优先，若需要 HTTP 则明确使用 MCP streamable HTTP/JSON-RPC，不写成 REST；浏览器 session 可复用但必须有 close/timeout。
- **现成 REST 档：** Crawl4AI `/crawl` 或 `/crawl/stream`。只映射稳定字段，不把 `/crawl/job` 的 queue、LLM hooks、任意 JS code 或 crawler traversal 暴露给模型；记录官方至少 4GB 是部署前提，不称作资源 benchmark。

### Phase 2：生产取向拆分

- **`web_search`：生产默认选 SearXNG；Websurfx 只作 Rust-only/实验对照。** Linux/server 优先使用官方 container 或受管远程实例；Windows 零前置依赖桌面版使用上节的 pinned embeddable-Python resource bundle，并把它视为 Stravia 维护的 downstream build，而不是上游支持的 native release。两者都声明 `remote/metasearch`，保留 upstream provenance 与 engine errors，明确“不是自有 index”；Websurfx 必须固定 rolling commit、验证 `json=true`/错误字段和 engine provenance 后才能决定是否采用。
- **`web_fetch` fast path：in-process native Rust。** 采用 `url` + RFC 9309 policy + 固定 robots parser + `scraper` + `dom_smoothie`；只做 network/robots/parse/extract/normalize。
- **`web_fetch` render fallback：chromiumoxide/外部 Chromium。** 只有静态正文为空/过短、检测为 SPA shell，或管理员 domain policy 强制 render 时才触发；若需要现成 REST，可把 Crawl4AI 作为 fetch-only sidecar。普通 URL 不支付 browser 成本。
- **若验收必须是一个服务同时承载两个 leaf：** 用 Firecrawl `/v2/search` + `/v2/scrape` 做成熟基准/PoC；明确 search 是上游集成而不是自有 index，并记录其整体 stack 不是轻量 fetch-only runtime。

### Phase 3：仅在要求自有 corpus 时增加 index

断网/自有 corpus 选择 YaCy local-only，或用 Spider + Tantivy 自建 bounded crawl/index。Stract 仅作架构参考；不要把 crawler/index 复杂度带入普通 `web_fetch`，也不要把 SearXNG/Websurfx/Firecrawl search route 误标为自有 index。

## 最小集成边界（不含实现代码）

### Dual-tool / render sidecar adapter

- Firecrawl adapter 同时返回 `supports_search() -> true` 与 `supports_fetch() -> true`；只调用受控 loopback REST `/v2/search`、`/v2/scrape`，不暴露 crawl/map/agent/interact 或任意 Playwright 执行能力。
- adapter 负责把 provider-native JSON 完整归一化为现有 response types；sidecar 错误、空结果、部分 fetch 失败、超时和 malformed JSON 都转成稳定 `WebAccessError`/`FetchResult`，不让其内部 schema 穿透 core。
- sidecar 不是安全边界。容器不得使用 host network，必须阻断 loopback、RFC1918、link-local、ULA、metadata endpoint 和内部 DNS；否则页面的 redirect、iframe、XHR、WebSocket 或 subresource 可绕过只检查顶层 URL 的 SSRF policy。
- Firecrawl 请求固定 `skipTlsVerification = false`；不接收调用者提供的 headers/cookies/proxy、actions、Interact prompt 或 Playwright code。限制浏览器并发、页面数、子资源、下载、CPU、内存、临时 profile、等待和总 deadline。

### Native fetch adapter

- 实现现有 `WebProviderAdapter`：`supports_fetch() -> true`，`supports_search() -> false`；`provider_id()` 为稳定 local ID。
- `fetch(&FetchRequest)` 返回与 `request.urls` 等长的 `Vec<FetchResult>`，包含原 URL、success/error/truncated、抽取正文和稳定安全错误；不得传出 raw response、环境变量、proxy credentials 或 stack trace。
- 复用 engine 的 count、character limit、deadline、batch fairness、fallback；adapter 自己补齐每跳 SSRF/DNS pinning、proxy policy、robots cache、body/decompression/resource limits。
- render fallback 只接收重新验证后的 URL 和固定 options；浏览器 sidecar 的所有网络流量受独立 egress policy 约束，不能依赖 native preflight 代替浏览器请求检查。

### Scrapling MCP adapter

- 只声明 `supports_fetch() -> true`、`supports_search() -> false`；默认通过 MCP stdio，或显式启用官方 `streamable-http` transport。调用方按 MCP JSON-RPC tool schema 传入 URL 与固定 extraction type，不把它包装成不存在的 REST `/fetch`。
- 只允许 `get`/`fetch`/`stealthy_fetch` 及必要 session lifecycle；禁止模型控制任意 `page_action`、headers/cookies/proxy、CDP endpoint 或执行脚本，除非这些选项由管理员策略固定。
- MCP HTTP transport 若启用，必须配置 auth token 与 allowed hosts/DNS-rebinding protection；browser session 的 close、timeout、max pages、profile 和 egress policy 都是 adapter 的责任。SearXNG 单独提供 `search`。

### Crawl4AI REST adapter

- 只声明 `supports_fetch() -> true`、`supports_search() -> false`；调用受控 `POST /crawl` 或 `/crawl/stream`，只映射 URL、等待/抽取模式、Markdown/HTML 和稳定错误字段；不暴露 `/crawl/job`、webhook、LLM hooks、任意 JS code 或深度 crawler traversal。
- 将官方 Docker 的 Redis/supervisor/Playwright 视为 sidecar 运维边界，不把它们传播进 core；启动时检查 health/auth，限制请求 body、browser pages、下载、CPU、内存、临时目录和 deadline。SearXNG 单独提供 `search`。

### Provider/config/admin 变更边界

- native HTTP provider 不能以 `api_key = None` 直接复用当前持久化 schema；必须新增 local kind，迁移 SQLite/PostgreSQL 的 kind/credential constraints，并更新 admin kind/API-key 校验和 capability。
- open-webSearch、SearXNG、Websurfx、Firecrawl、Scrapling MCP、Crawl4AI 或 YaCy adapter 必须从经过校验的 `base_url/options` 或受控子进程配置读取 endpoint，不能硬编码 localhost，也不能接受未经 policy 的任意 URL；因此需要持久化 schema、Admin API、WebUI 配置、运行时 snapshot 和 migration/compatibility 设计。
- 运行时 adapter 仍只通过 `WebProviderAdapter::search/fetch` 暴露；crawler scheduler、index lifecycle、browser orchestration 属于 sidecar/后台 capability，不传播其类型到 core。

### Search adapter

open-webSearch、SearXNG 或 Websurfx adapter 将 query/max_results/domain filters 映射到各自 API，再在 WebAccess seam 做 URL normalization/post-filter，并明示 upstream engines；SearXNG adapter 按正式 Search API 作为生产默认，Websurfx adapter 必须固定 rolling commit 并验证 `json=true`/`engineErrorsInfo`/provenance 后才能进入实验配置。YaCy/Tantivy adapter 只在自有 corpus 模式下返回 `SearchResponse { mode: Index, query, results }`，把 index revision/crawl timestamp 留在内部 provenance，不扩张旧 public result schema 未定义字段。Web Research hidden leaves 仍通过 WebAccessService 使用 provider snapshot/fallback。

## PoC 验收标准

### A. Native fetch 功能和安全

1. 覆盖 1/20 URL、空/重复/credential/non-HTTP；每输入一个结果且顺序不变，`max_characters` 与 64,000 总上限有效。
2. public host redirect 到 loopback、RFC1918、link-local、IPv6 special、`.local`、不同 scheme；每跳拒绝。测试首次 DNS public、后续 private 的 rebinding，证明实际连接未落入 private（DNS pinning/受控 resolver），不能只验证 preflight log。
3. 分别设置系统 proxy、管理员 proxy、`NO_PROXY`；证明 proxy 不绕过 policy，记录 egress decision。
4. 超大 body、gzip/brotli bomb、慢 header/body、无限 redirect、错误 charset、非 HTML MIME、connection reset 都产生稳定 error/truncated；不泄露 body/secret。
5. RFC 9309 fixture 覆盖 UA、Allow/Disallow longest match、redirect/unavailable/unreachable、cache、parse error；robots 拒绝与 SSRF 拒绝是不同错误。
6. 正文、导航噪声、表格、代码块、中文/多语、无 `<article>`、仅 metadata、正文为空 fixture；比较 dom_smoothie 与 fallback，验证 title/text/URL、截断和 limitations。
7. JS-heavy 页面先走 native fast path；只有正文为空/过短、SPA shell 或显式 domain policy 才触发 render。fallback 超时/崩溃安全返回，且不重复已经成功的 URL；browser process/profile/network 资源可回收。
8. native adapter 对部分 URL 稳定失败时，下一个 provider 只接手 pending URL；不重复成功 URL，保持顺序和共享 deadline。

### B. Local/metasearch search

1. SearXNG Docker/Podman 受控实例启用 JSON API，关闭/替换 upstream engine；结果显示 upstream 缺失，不误报 local index；验证 query、domain filter、timeout、429/engine error、provenance；同时验证只启用 `search.formats: [json]` 时 HTML/未启用 format 的 403 行为。
2. Websurfx Docker/源码受控实例固定 rolling commit；分别请求 `GET /search?q=...&json=true&page=1&safesearch=...`、空 query、未知参数、`format=json=true`；记录真实 status/content-type，并断言 README 示例与源码行为差异；注入一个失败 upstream，验证 `engineErrorsInfo`、部分结果、错误映射与 provenance 不错配。关闭 memory/Redis cache、改 proxy/rate limit 后重复测试，确认 adapter 不依赖隐含默认值。
3. YaCy 固定 seed/domain、关闭 P2P，重启后只用 local index 查询；验收 provenance、crawl timestamp、更新/删除、robots/rate、磁盘持久化；另测 P2P 开启时来源变化。
4. Rust route 用 Spider 抓 bounded corpus、写 Tantivy；验收 commit/reopen、canonical/dedup、ranking、recrawl/delete、crash recovery 和 URL provenance；不得把未测 pages/s 或相关性写成 benchmark。
5. 采样测量平均响应/正文 bytes、render ratio、有效 pages/s、index bytes/page、recrawl cost、失败率；把实测和本文估算分开。

### C. JS-heavy fetch process/REST and dual-tool baseline

1. **Scrapling MCP：** stdio 与 streamable HTTP 两种 transport 都 smoke test；验证 `get`/`fetch`/`stealthy_fetch` 返回 status/url 与 Markdown/HTML/text，MCP session 可关闭、超时、回收 browser；断言没有 `search` tool，search 请求走 SearXNG。
2. **Crawl4AI：** Docker 单容器执行 `/crawl` 与 `/crawl/stream`，验证 Markdown/HTML/structured extraction、auth/health、超时和 malformed response；不把 `/crawl/job`、webhook、Redis queue、LLM hooks 或任意 crawler traversal 传入 leaf。记录官方“至少 4GB”部署前提，不写成实测资源结论。
3. **Firecrawl baseline：** 同一 self-host 实例分别执行 `/v2/search` 与 `/v2/scrape`；验证 search 结果 URL 可直接交给 fetch，且 final URL、title、snippet/content、provider、error 和 truncation 可归一化；在报告中标明 search 不是自有 index。
4. fixture 至少覆盖静态正文、CSR SPA、延迟 XHR、infinite scroll shell、重定向后渲染、脚本错误和永不完成的网络请求；证明只有需要 JS 的 URL 启动 Playwright/Chromium。
5. 用顶层/redirect/iframe/XHR/WebSocket/subresource 指向 loopback、RFC1918、link-local、ULA 和 metadata endpoint 的页面验证 egress 隔离；显式断言 TLS verification 不被关闭，拒绝自签名/主机名不匹配证书。
6. 禁用/破坏 search upstream、Playwright worker、queue 或 database（对适用组件），确认部分结果、稳定错误和下一 provider fallback；sidecar 返回 malformed JSON/MCP payload 或超时时不泄露内部 stack/路径。
7. Docker 环境执行 smoke test，记录冷启动、静态 fast-path 与 render fallback 延迟、内存、浏览器并发和临时目录回收；这些是本地验收数据，不得写成通用 benchmark。

## 一手来源（均访问于 2026-08-17）

### Stravia 现状

- [Web Search 设计](../design/web-search.md)
- [ADR-0002：Web Access provider seam](../adr/0002-web-access-provider-seam.md)
- [`stravia-core::web_access::mod.rs`](../../backend/crates/stravia-core/src/web_access/mod.rs)
- [`stravia-core::web_access::providers.rs`](../../backend/crates/stravia-core/src/web_access/providers.rs)

### Rust fetch / parse / render

- [reqwest 官方仓库](https://github.com/seanmonstar/reqwest)；[`ClientBuilder`](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)；[redirect](https://docs.rs/reqwest/latest/reqwest/redirect/index.html)
- [url docs.rs / WHATWG URL implementation](https://docs.rs/url/latest/url/)
- [Google robots.txt Rust port `robotstxt`](https://docs.rs/robotstxt/latest/robotstxt/)；[crate metadata](https://docs.rs/crate/robotstxt/latest)
- [`robots_txt`](https://docs.rs/robots_txt/latest/robots_txt/)；[crate metadata/README](https://docs.rs/crate/robots_txt/latest)
- [IETF RFC 9309](https://www.rfc-editor.org/rfc/rfc9309)
- [scraper docs.rs](https://docs.rs/scraper/latest/scraper/)；[rust-scraper repo](https://github.com/rust-scraper/scraper)
- [dom_smoothie crate](https://docs.rs/crate/dom_smoothie/latest)；[official repo](https://github.com/niklak/dom_smoothie)
- [readability crate](https://docs.rs/crate/readability/latest)
- [headless_chrome docs.rs](https://docs.rs/headless_chrome/latest/headless_chrome/)；[official Cargo manifest](https://raw.githubusercontent.com/rust-headless-chrome/rust-headless-chrome/master/Cargo.toml)
- [chromiumoxide repo](https://github.com/mattsse/chromiumoxide)；[crate metadata](https://docs.rs/crate/chromiumoxide/latest)
- [Playwright supported languages](https://playwright.dev/docs/languages)
- [Spider repo/README](https://github.com/spider-rs/spider)；[crate metadata](https://docs.rs/crate/spider/latest)

### 国内社交媒体与开源候选

- [微信服务号草稿箱官方文档](https://developers.weixin.qq.com/doc/service/guide/product/draft.html)；[素材管理](https://developers.weixin.qq.com/doc/service/guide/product/asset.html)；[发布能力](https://developers.weixin.qq.com/doc/service/guide/product/publish.html)
- [小红书官方开放平台入口](https://open.xiaohongshu.com/)；[官方文档入口](https://open.xiaohongshu.com/document/developer/file/53)
- [SearXNG settings.yml](https://raw.githubusercontent.com/searxng/searxng/master/searx/settings.yml)；[sogou_wechat.py](https://raw.githubusercontent.com/searxng/searxng/master/searx/engines/sogou_wechat.py)；[Sogou WeChat public HTML PoC](https://weixin.sogou.com/weixin?type=2&query=%E4%BA%BA%E5%B7%A5%E6%99%BA%E8%83%BD&page=1)
- [xpzouying/xiaohongshu-mcp README](https://raw.githubusercontent.com/xpzouying/xiaohongshu-mcp/main/README.md)；[xiaohongshu/search.go](https://raw.githubusercontent.com/xpzouying/xiaohongshu-mcp/main/xiaohongshu/search.go)；[LICENSE](https://raw.githubusercontent.com/xpzouying/xiaohongshu-mcp/main/LICENSE)；[GitHub API metadata](https://api.github.com/repos/xpzouying/xiaohongshu-mcp)
- [NanmiCoder/MediaCrawler README](https://raw.githubusercontent.com/NanmiCoder/MediaCrawler/main/README.md)；[LICENSE](https://raw.githubusercontent.com/NanmiCoder/MediaCrawler/main/LICENSE)；[GitHub API metadata](https://api.github.com/repos/NanmiCoder/MediaCrawler)
- [wechat-article/wechat-article-exporter README](https://github.com/wechat-article/wechat-article-exporter)；[LICENSE](https://raw.githubusercontent.com/wechat-article/wechat-article-exporter/master/LICENSE)；[GitHub API metadata](https://api.github.com/repos/wechat-article/wechat-article-exporter)
- [jj-cheng25/weixin-articles-mcp README](https://github.com/jj-cheng25/weixin-articles-mcp)；[server.py](https://raw.githubusercontent.com/jj-cheng25/weixin-articles-mcp/main/src/weixin_articles_mcp/server.py)；[LICENSE](https://raw.githubusercontent.com/jj-cheng25/weixin-articles-mcp/main/LICENSE)；[GitHub API metadata](https://api.github.com/repos/jj-cheng25/weixin-articles-mcp)
- [fengxxc/wechatmp2markdown README/source](https://github.com/fengxxc/wechatmp2markdown)；[main.go](https://raw.githubusercontent.com/fengxxc/wechatmp2markdown/master/main.go)；[LICENSE](https://raw.githubusercontent.com/fengxxc/wechatmp2markdown/master/LICENSE)

### 轻量 JS-heavy 候选

- [`chromiumoxide` README](https://raw.githubusercontent.com/mattsse/chromiumoxide/main/README.md)；[`Cargo.toml`](https://raw.githubusercontent.com/mattsse/chromiumoxide/main/Cargo.toml)；[`fetcher/platform.rs`](https://raw.githubusercontent.com/mattsse/chromiumoxide/main/chromiumoxide_fetcher/src/platform.rs)
- [`headless_chrome` README](https://raw.githubusercontent.com/rust-headless-chrome/rust-headless-chrome/master/README.md)；[`Cargo.toml`](https://raw.githubusercontent.com/rust-headless-chrome/rust-headless-chrome/master/Cargo.toml)
- [`Scrapling` fetcher comparison](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/docs/fetching/choosing.md)；[`dynamic.md`](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/docs/fetching/dynamic.md)；[MCP API reference](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/docs/api-reference/mcp-server.md)；[`core/ai.py`](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/scrapling/core/ai.py)；[`pyproject.toml`](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/pyproject.toml)；[`Dockerfile`](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/Dockerfile)；[`LICENSE`](https://raw.githubusercontent.com/D4Vinci/Scrapling/main/LICENSE)
- [`Crawl4AI` README](https://raw.githubusercontent.com/unclecode/crawl4ai/main/README.md)；[Docker guide/API](https://raw.githubusercontent.com/unclecode/crawl4ai/main/deploy/docker/README.md)；[`Dockerfile`](https://raw.githubusercontent.com/unclecode/crawl4ai/main/Dockerfile)；[`pyproject.toml`](https://raw.githubusercontent.com/unclecode/crawl4ai/main/pyproject.toml)；[`GoogleSearchCrawler`](https://raw.githubusercontent.com/unclecode/crawl4ai/main/crawl4ai/crawlers/google_search/crawler.py)；[`LICENSE`](https://raw.githubusercontent.com/unclecode/crawl4ai/main/LICENSE)
- [`Lightpanda` README](https://raw.githubusercontent.com/lightpanda-io/browser/main/README.md)；[`LICENSE`](https://raw.githubusercontent.com/lightpanda-io/browser/main/LICENSE)
- [`Browserless` README/licensing](https://raw.githubusercontent.com/browserless/browserless/main/README.md)；[official Search API](https://docs.browserless.io/rest-apis/search)

### Rust / self-hosted search and crawler/index

- [Tantivy repo/README](https://github.com/quickwit-oss/tantivy)；[docs.rs](https://docs.rs/tantivy/latest/tantivy/)
- [Meilisearch repo/README](https://github.com/meilisearch/meilisearch)
- [Stract repo/README](https://github.com/StractOrg/stract)；[LICENSE](https://raw.githubusercontent.com/StractOrg/stract/main/LICENSE.md)；[API metadata](https://api.github.com/repos/StractOrg/stract)；[setup](https://raw.githubusercontent.com/StractOrg/stract/main/CONTRIBUTING.md)
- [SearXNG repo/README](https://github.com/searxng/searxng)；[`master/searx/settings.yml`](https://raw.githubusercontent.com/searxng/searxng/master/searx/settings.yml)；[engine settings semantics](https://docs.searxng.org/admin/settings/settings_engines.html)；[Search API](https://docs.searxng.org/dev/search_api.html)；[configured engines](https://docs.searxng.org/user/configured_engines.html)；[container installation](https://docs.searxng.org/admin/installation-docker.html)；[CHANGELOG](https://raw.githubusercontent.com/searxng/searxng/master/CHANGELOG.rst)；[`requirements.txt`](https://raw.githubusercontent.com/searxng/searxng/master/requirements.txt)；[`search/__init__.py`](https://raw.githubusercontent.com/searxng/searxng/master/searx/search/__init__.py)；[`webutils.py`](https://raw.githubusercontent.com/searxng/searxng/master/searx/webutils.py)；[search settings](https://docs.searxng.org/admin/settings/settings_search.html)；[engine settings](https://docs.searxng.org/admin/settings/settings_engines.html)；[outgoing](https://docs.searxng.org/admin/settings/settings_outgoing.html)；[limiter](https://docs.searxng.org/admin/searx.limiter.html)；[plugins](https://docs.searxng.org/admin/settings/settings_plugins.html)；[Valkey](https://docs.searxng.org/admin/settings/settings_valkey.html)
- [SearXNG Windows sidecar 补充来源：`/healthz` route](https://raw.githubusercontent.com/searxng/searxng/master/searx/webapp.py)；[`requirements-server.txt`](https://raw.githubusercontent.com/searxng/searxng/master/requirements-server.txt)；[Granian installation](https://docs.searxng.org/admin/installation-granian.html)；[settings `use_default_settings` / `keep_only`](https://docs.searxng.org/admin/settings/settings.html#use-default-settings)；[CPython Windows embeddable package](https://docs.python.org/3/using/windows.html#the-embeddable-package)；[Tauri sidecar](https://v2.tauri.app/develop/sidecar/)；[Tauri config schema `bundle.resources`](https://schema.tauri.app/config/2)
- [YaCy repo/README](https://github.com/yacy/yacy_search_server)；[API metadata](https://api.github.com/repos/yacy/yacy_search_server)
- [Apache Nutch repo/README](https://github.com/apache/nutch)；[Docker README](https://raw.githubusercontent.com/apache/nutch/master/docker/README.md)
- [Common Crawl: June 2026 Crawl Archive](https://commoncrawl.org/blog/june-2026-crawl-archive-now-available)

### leaf search / fetch 实现

- [`open-webSearch` 官方 README](https://raw.githubusercontent.com/Aas-ee/open-webSearch/main/README.md)；[`package.json`](https://raw.githubusercontent.com/Aas-ee/open-webSearch/main/package.json)；[local daemon HTTP API](https://raw.githubusercontent.com/Aas-ee/open-webSearch/main/docs/http-api.md)；[releases feed](https://github.com/Aas-ee/open-webSearch/releases)
- [`Websurfx` rolling README](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/README.md)；[`websurfx/config.lua`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/websurfx/config.lua)；[`src/engines/mod.rs`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/src/engines/mod.rs)；[`src/models/engine.rs`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/src/models/engine.rs)；[`Cargo.toml`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/Cargo.toml)；[`src/routes/search.rs`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/src/routes/search.rs)；[`src/models/search_route.rs`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/src/models/search_route.rs)；[release v1.29.9](https://github.com/neon-mmd/websurfx/releases/tag/v1.29.9)；[`LICENSE`](https://raw.githubusercontent.com/neon-mmd/websurfx/rolling/LICENSE)；[`src/models/aggregation.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/aggregation.rs)；[`src/aggregator.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/aggregator.rs)；[`src/models/engine.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/models/engine.rs)；[`websurfx/config.lua`](https://github.com/neon-mmd/websurfx/blob/rolling/websurfx/config.lua)；[`src/cache/mod.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/cache/mod.rs)；[`src/cache/memory.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/cache/memory.rs)；[`src/cache/redis.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/cache/redis.rs)；[`src/lib.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/lib.rs)；[`src/main.rs`](https://github.com/neon-mmd/websurfx/blob/rolling/src/main.rs)；[`Dockerfile`](https://github.com/neon-mmd/websurfx/blob/rolling/Dockerfile)；[CI](https://github.com/neon-mmd/websurfx/blob/rolling/.github/workflows/rust.yml)；[configuration](https://github.com/neon-mmd/websurfx/blob/rolling/docs/configuration.md)
- [`Firecrawl` 官方仓库/README](https://github.com/firecrawl/firecrawl)；[self-host guide](https://docs.firecrawl.dev/contributing/self-host)；[`POST /v2/search` API](https://docs.firecrawl.dev/api-reference/endpoint/search)；[`POST /v2/scrape` API](https://docs.firecrawl.dev/api-reference/endpoint/scrape)
- [`xynehq/websearch` 官方仓库/README](https://github.com/xynehq/websearch)；[`Cargo.toml`](https://raw.githubusercontent.com/xynehq/websearch/main/Cargo.toml)
