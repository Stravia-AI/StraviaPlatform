# metasearch2（metasearch-rust）作为 Stravia Web Access 自托管搜索 backend 的一手研究

| 项 | 值 |
|---|---|
| 研究日期 | 2026-08-20（事实按此日期截断） |
| 研究范围 | `mat-1/metasearch2` 仓库 README、Rust 源码、`Cargo.toml`、`LICENSE`、`Containerfile`、`compose.yml`、GitHub 仓库元数据/commits/releases/issues，以及 crates.io、docs.rs 官方页面 |
| 目标 | 评估 crate `metasearch`（项目名 metasearch2）是否适合作为 Stravia Web Access 的自托管搜索 backend，并确定 JSON API 与现有 adapter seam 的适配工作量 |
| 核心边界 | metasearch2 是 HTTP 抓取/解析并汇总多个上游搜索站点的独立服务；自托管只改变 Stravia 到该服务的第一跳，不会使查询停止发往 Google、Bing、Brave 等上游；它没有独立的正文 fetch/reader API |

> 本文只把项目自身或其官方发布页面明确写出的内容当作事实。对代码未实现、官方未保证或由事实推导的内容，明确标为“源码未发现”或“评估”。关键事实以 `[S#]` 标注；JSON API 的稳定性则沿用项目 README 自身的警告。[S1]

## 1. 先给结论

1. **可以做成一个很薄的搜索-only adapter，但不应直接作为 Stravia Web Access 的等价替代。** metasearch2 的搜索接口是 `GET /search`，在配置 `api = true` 且请求带精确的 `Accept: application/json`（或 `format=json`）时返回 JSON；核心结果可映射为 Stravia `SearchResult { url, title?, snippet? }`。[S1][S12][S19]
2. **API 默认关闭，且响应是不稳定的内部 Rust struct 序列化。** 默认 `api = false`；关闭时 API 请求得到 `403 API access is disabled`。README 明确说 JSON 结构可能无预告变更，因此必须在 adapter 中做严格解析、版本/字段监控和失败降级，而不能把它当长期稳定公共协议。[S1][S12]
3. **它不是 API-key SaaS 聚合器，而是服务器侧抓取器。** 当前源码把 Google、Bing、Brave、Google Scholar、Marginalia、RightDao、Stract 的 HTML 页面抓下来再用 CSS selector 解析；Yep 使用 `https://api.yep.com/fs/2/search` JSON 接口。默认启用 Google/Bing/Brave/Marginalia，其他搜索引擎默认禁用。[S9][S10][S11]
4. **架构足够轻，但可靠性取决于上游 HTML 与反滥用政策。** 每个启用引擎并发请求，单个 HTTP client timeout 为 10 秒；成功响应采用 `1/(结果位置)` × 引擎 `weight` × URL weight 的加权合并，同 URL 去重并按总分排序。没有 provider API 的稳定性、配额或 schema 保障。[S9][S12][S13]
5. **维护状态不是“已停止”，但发布节奏明显落后于源码。** 仓库截至研究日期未 archived，GitHub 元数据有 14 个 open issues；最新 master commit 是 2026-07-12。GitHub releases API 返回空列表、仓库也没有 tags；crates.io/docs.rs 最新发布版本仍是 `0.2.4`（2025-07-06）。因此应 pin Git commit 或验证发布 crate，不能假定两者一致。[S2][S3][S4][S5][S6]
6. **已有一手 issue 证明抓取脆弱性是实际问题，不只是理论风险。** open issue #24 报告 Bing 运行一段时间后会返回与查询无关的结果；issue #46 报告 Google/Bing 图片解析失败；open PR #31 的描述称 Google HTML 搜索“no longer works”并提出改用 Custom Search API（该 PR 未合并，故不能当作当前代码契约）。README 还直接提醒公共 demo 可能被 Google 等上游 rate-limit；issue #33 记录关闭 autocomplete 以降低 rate-limit 机会。[S1][S15][S16][S17][S18]
7. **与 Stravia seam 的最大缺口是 `fetch`。** Stravia 的 `WebProviderAdapter` 同时要求 `search` 和 `fetch`；metasearch2 只有搜索 JSON/HTML UI、autocomplete、image proxy 等路由，没有独立正文读取接口。它最多作为只实现 `supports_search()` 的 adapter，fetch 必须继续由另一 provider 提供，或由 Stravia 保持现有 fetch 路径。[S12][S19][S20]
8. **建议：仅作为明确 opt-in、搜索-only、可观测的实验 backend，不作为默认生产 provider。** 采用前至少应固定上游引擎 allowlist、限制 metasearch2 网络出口、监控空结果/重复结果/异常引擎分布、对 API 结构做 contract probe，并准备切回现有 Exa/Brave/Tavily/Zhipu。自托管并不消除抓取 ToS、CAPTCHA、rate limit、结果质量或上游出境风险。[S1][S2][S9][S12]

## 2. 来源矩阵

| ID | 一手来源 | 本文采用的事实 |
|---|---|---|
| S1 | [仓库 README](https://github.com/mat-1/metasearch2/blob/master/README) / [raw README](https://raw.githubusercontent.com/mat-1/metasearch2/master/README) | 项目定位、上游来源概述、公共 demo 的日志和 rate-limit 警告、`cargo install`/git 安装、配置搜索路径、默认端口 `28019`、API 开启方式与不稳定性声明 |
| S2 | [GitHub repository API](https://api.github.com/repos/mat-1/metasearch2) | `archived=false`、`open_issues=14`、默认分支 `master`、stars/forks、仓库 license 元数据、`has_downloads=false`、最近 push 时间 |
| S3 | [GitHub latest commit API](https://api.github.com/repos/mat-1/metasearch2/commits?per_page=1) | 最新 master commit `33c0b4b...`，提交时间 2026-07-12，消息为 `improved ssrf protection for image proxy` |
| S4 | [GitHub releases API](https://api.github.com/repos/mat-1/metasearch2/releases?per_page=10) / [tags API](https://api.github.com/repos/mat-1/metasearch2/tags?per_page=20) | 截至研究日期 releases 与 tags 均为空；没有仓库 release/tag 可供部署 pin |
| S5 | [crates.io `metasearch`](https://crates.io/crates/metasearch) | 最新 crates.io 版本 `0.2.4`、许可证 CC0-1.0、版本日期 2025-07-06、历史版本日期 |
| S6 | [docs.rs `metasearch` latest](https://docs.rs/crate/metasearch/latest) | docs.rs 当前 latest 为 `0.2.4`；页面列出的发布日期、仓库链接、依赖与“不是 library”的 binary crate 说明 |
| S7 | [Cargo.toml](https://raw.githubusercontent.com/mat-1/metasearch2/master/Cargo.toml) | crate 名 `metasearch`、当前仓库版本字段 `0.2.4`、edition 2021、license `CC0-1.0`、仓库地址、HTTP/Rust 依赖 |
| S8 | [默认配置实现 `src/config.rs`](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/config.rs) / [config-default.toml](https://raw.githubusercontent.com/mat-1/metasearch2/master/config-default.toml) | 默认 bind/API/UI/image search/engine 配置、engine `enabled`/`weight`/`extra` 结构、配置 overlay 规则 |
| S9 | [引擎注册与请求分发 `src/engines/mod.rs`](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/mod.rs) | 搜索引擎清单、请求/解析函数注册、启用引擎并发 fan-out、单 HTTP client 10 秒 timeout、错误引擎被跳过、`EngineResponse`/`Response` 序列化字段 |
| S10 | [HTML 引擎实现目录](https://github.com/mat-1/metasearch2/tree/master/src/engines/search)；代表实现：[Google](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/google.rs)、[Bing](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/bing.rs)、[Brave](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/brave.rs)、[Marginalia](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/marginalia.rs)、[Stract](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/stract.rs) | Google/Bing/Brave/Marginalia/Stract 等通过上游 HTML URL + `scraper` CSS selector 解析；请求 URL 与关键 selector；Google/Bing 图片解析的脆弱内部 JSON/文本逻辑 |
| S11 | [Yep 引擎实现](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/search/yep.rs)；[ranking.rs](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/engines/ranking.rs) | Yep 的 JSON endpoint/字段；加权 reciprocal-rank 风格合并、同 URL 去重、engine 集合和 score 字段、URL 替换/权重 |
| S12 | [JSON API handler `src/web/search.rs`](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/web/search.rs) | `GET /search`、`q`/`tab` 参数、`Accept: application/json` 与 `format=json` 判断、API 关闭时 403、返回 `Json(Vec<ResponseForTab>)`、HTML 默认路径 |
| S13 | [Containerfile](https://raw.githubusercontent.com/mat-1/metasearch2/master/Containerfile) / [compose.yml](https://raw.githubusercontent.com/mat-1/metasearch2/master/compose.yml) | Rust release builder + distroless runtime、EXPOSE `28019`、binary entrypoint、host networking、restart 策略 |
| S14 | [仓库 LICENSE](https://raw.githubusercontent.com/mat-1/metasearch2/master/LICENSE) | LICENSE 文件完整标题为 CC0 1.0 Universal，并含“as-is”及不负责清理第三方权利的免责声明 |
| S15 | [issue #24](https://api.github.com/repos/mat-1/metasearch2/issues/24) | open issue 报告 Bing 长时间运行后返回完全无关查询的结果，并指出 demo 未使用 Bing |
| S16 | [open PR/issue #31](https://api.github.com/repos/mat-1/metasearch2/issues/31) | 未合并提案称 Google 搜索“不再工作”，建议 Custom Search API；仅作为维护/脆弱性信号，不改写当前源码事实 |
| S17 | [issue #46](https://api.github.com/repos/mat-1/metasearch2/issues/46) 与 [open PR #50](https://api.github.com/repos/mat-1/metasearch2/issues/50) | issue 日志中的 Google image internal JSON 解析错误、Bing 尺寸解析警告；PR #50 声称修复 Bing image search 且引用 #46，PR 尚未合并 |
| S18 | [issue #33](https://api.github.com/repos/mat-1/metasearch2/issues/33) | 用户为降低 rate-limit 机会而请求关闭 autocomplete；仓库随后实现 `ui.show_autocomplete` 配置 |
| S19 | [web server `src/web/mod.rs`](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/web/mod.rs) / [main.rs](https://raw.githubusercontent.com/mat-1/metasearch2/master/src/main.rs) | 路由注册、bind、配置文件参数、没有 API 认证中间件的源码路径、运行命令 |
| S20 | [Stravia `web_access/mod.rs`](../../backend/crates/stravia-core/src/web_access/mod.rs) / [providers.rs](../../backend/crates/stravia-core/src/web_access/providers.rs) | 本仓库 seam：`SearchResponse`、`SearchResult`、`WebProviderAdapter` 的 search/fetch 双能力，以及现有 Exa/Brave/Tavily/Zhipu `AdapterConfig` 模式；不是 metasearch2 一手来源 |

## 3. 项目概况与维护状态

### 3.1 项目身份与发布面

仓库将自身描述为“a cute metasearch engine”，crate 名为 `metasearch`，Rust edition 为 2021，当前仓库 `Cargo.toml` 版本字段为 `0.2.4`，license 字段为 `CC0-1.0`。[S1][S7] docs.rs 将它标为 binary crate 而不是 library；因此 Stravia 应把它视为需要独立进程/服务边界的 HTTP backend，而不是链接进 `stravia-core` 的 Rust library。[S6]

GitHub 仓库元数据截至本研究读取为：未 archived、14 个 open issues、默认分支 `master`、171 stars、32 forks；GitHub API 的 releases 和 tags 列表均为空。[S2][S4] 最新 master commit 是 `33c0b4b330e2f0cb13161a80cd80bed9f2c3008e`，作者提交时间 2026-07-12T18:35:27Z，消息是改进 image proxy 的 SSRF 防护。[S3] 这说明主仓库在研究日期前仍有近期提交，不能称为“项目停止维护”；但 GitHub 没有可部署 release/tag，发布渠道与源码 HEAD 存在可见差异。

crates.io 页面与 docs.rs latest 都显示 `0.2.4`，发布日期 2025-07-06；仓库 HEAD 的 `Cargo.toml` 仍为 `0.2.4`，但依赖版本约束已与 docs.rs 页面列出的 2025 发布包依赖不同（例如仓库当前使用 `axum 0.8.7`、`wreq`，而 docs.rs 页面列的是已发布包的依赖集合）。[S5][S6][S7] 这不是版本号本身变化的证据，而是采用 `cargo install metasearch` 与从 Git 安装可能得到不同依赖图的证据；部署必须记录 lockfile、来源和 commit，并在 CI 构建探针中验证。

### 3.2 Issue 信号

- issue #24（open）报告 Bing 在运行一段时间后开始返回“完全无关搜索”的结果，且难以复现；作者观察到 demo 也未使用 Bing。[S15]
- issue #46（open）给出实际日志：Google 图片解析报 `couldn't get internal json for google images`，Bing 图片解析大量报告无法从返回文本解析宽高；open PR #50 声称修复该问题，但截至研究日期仍未合并。[S17]
- open PR #31 的正文称 Google 搜索“no longer works”，并提议默认禁用 HTML Google、改用需要 `custom_search_api_key` 的 Custom Search API。因为该 PR 的 `merged_at` 为 null，当前 master 源码仍应以 `google.com/search` HTML parser 为准；该条只能作为维护者/贡献者观察到的失效信号。[S16]
- issue #33 明确把关闭 autocomplete 与“decrease the rate limit chance”联系起来；README 也要求不要依赖公共 demo，因为作者会被 Google 等引擎 rate-limit。[S1][S18]

综合判断：主仓库活跃性为“近期有 commit、未 archived”，但 upstream HTML 变化造成的质量故障已有 open issue，且 crate release 落后；适合作为需持续维护的实验依赖，不适合作为无需运营投入的稳定搜索 SLA。

## 4. 架构与引擎实现

### 4.1 请求生命周期

`src/engines/mod.rs` 为每个 `Engine::all()` 检查 `config.engines.get(engine).enabled`，对启用引擎调用其 `request`，再由 `join_all` 并行等待。HTTP 请求使用全局 `wreq::Client`，伪装 Firefox 139，并设置 `timeout(Duration::from_secs(10))`；下载整个 body 后再解析。单引擎 request、HTTP 或 parse 失败都会记录错误并不进入成功响应集合，随后继续合并其他成功引擎。[S9]

如果没有 infobox，代码还会触发 post-search 引擎请求；这属于 UI 的额外行为，不应被 Stravia adapter 依赖。主 `/search` API 收集搜索完成时发出的 `Response` progress update，最终以 JSON 返回；不会把每个上游原始 response 直接暴露出来。[S9][S12]

### 4.2 搜索引擎清单与实现方式

`src/engines/mod.rs` 注册的主搜索引擎为：`Google`、`GoogleScholar`、`Bing`、`Brave`、`Marginalia`、`RightDao`、`Stract`、`Yep`。[S9]

| 引擎 | 当前实现 | 默认状态/权重 | 观察 |
|---|---|---:|---|
| `Google` | `GET https://www.google.com/search?...`，`scraper` selector 解析 HTML | enabled / `1.05` | Google 结果 selector 依赖页面 DOM；图片搜索还解析内部 JSON |
| `Bing` | `GET https://www.bing.com/search?...`，解析 HTML | enabled / `1.0` | 通过 `cvid`、cookie 和固定 query 参数构造浏览器式请求；图片另走 HTML/属性 JSON |
| `Brave` | `GET https://search.brave.com/search?q=...`，解析 HTML | enabled / `1.25` | 无 API key；依赖 `.snippet` 等 DOM selector |
| `Marginalia` | `GET https://old-search.marginalia.nu/search`，解析 HTML | enabled / `0.15` | 仅在 query 不超过 3 个单词且仅 ASCII 字母/空格时请求；使用 `profile/js/adtech` extra |
| `GoogleScholar` | `GET https://scholar.google.com/scholar?...`，解析 HTML | disabled / `0.50` | 禁用需显式启用 |
| `RightDao` | `GET https://rightdao.com/search?q=...`，解析 HTML | disabled / `0.10` | 禁用需显式启用 |
| `Stract` | `GET https://stract.com/search?...`，解析 HTML | disabled / `0.15` | 禁用需显式启用 |
| `Yep` | `GET https://api.yep.com/fs/2/search?...`，反序列化 JSON | disabled / `0.10` | 非 HTML；使用无 API key 的上游 JSON endpoint，解析顶层 tuple `(code, response)` |

表中 URL、selector、限制与权重均来自本次读取的源码，不代表上游服务承诺的稳定 API。[S8][S9][S10][S11]

仓库还注册 answer engines（如 `Numbat`、`Fend`、`Dictionary`、`Wikipedia`）及 post-search engines（如 `DocsRs`、`GitHub`、`Mdn`、`StackExchange`），但它们不是通用 Web Search backend 的等价结果源；`Engine::all()` 仍由统一配置控制。[S9]

### 4.3 结果合并与排序

`ranking::merge_engine_responses` 对每个上游结果按零基索引计算：

\[
  score = \frac{1}{position + 1} \times engine.weight \times url.weight
\]

代码将 URL replacements 先应用，再读取 URL weight；`url.weight <= 0` 的结果被丢弃。相同 URL 的结果合并，`engines` 集合记录贡献来源，score 相加；如果当前引擎 weight 高于已合并来源的最高 weight，则覆盖 title/description。最后按 score 降序排序。[S11]

这是一种项目自定义的加权 reciprocal-rank 风格融合，不是可配置的学习排序或外部 rank-fusion 服务；结果带有项目内部 `engines` 与 `score` 元数据。[S11] 源码中没有缓存层、持久化搜索索引或独立抓取队列；在当前实现下，一个 query 会即时向启用的上游发请求（“未发现缓存”是源码审阅结论，不宣称项目对所有部署变体绝对不存在缓存）。[S9][S11]

## 5. JSON API 契约（字段级）

### 5.1 开启方式、endpoint 与请求参数

路由在 `src/web/mod.rs` 注册为 `GET /search`。[S19] handler 先读取 query string：

- `q`：搜索词；缺失或 trim 后为空时返回 `302` 到 `/`，不是 JSON 搜索结果。
- `tab`：可为 `all` 或 `images`；默认 `all`。图片 tab 还要求 `image_search.enabled=true`，否则搜索会报 unknown tab。[S8][S9][S12]
- API 判断为请求 header 中的 `Accept` **严格等于** `application/json`，或 query 参数 `format=json`。README 推荐前一种写法：[S1][S12]
- 只有配置 `api = true` 才开放。默认配置和 `Config::default()` 都是 `false`；未开启时返回 HTTP `403`，body 为 `API access is disabled`。[S8][S12]

README 给出的调用为：

```sh
curl 'http://localhost:28019/search?q=sandcats' -H 'Accept: application/json'
```

默认 bind 为 `0.0.0.0:28019`；README 建议使用 reverse proxy。[S1][S8] 源码没有 API key、Basic auth、Bearer auth 或用户级 rate-limit 中间件；API 一旦开启，访问控制应由 reverse proxy、网络 ACL 或 Stravia 侧网络策略补上。[S12][S19]

### 5.2 响应 JSON 的实际 shape

API handler 启动 `engines::search`，收集 `ProgressUpdateData::Response`，最后执行 `Json(results)`，其中 `results` 的类型是 `Vec<ResponseForTab>`。[S12] `ResponseForTab` 是 `#[serde(untagged)]`，所以 JSON 顶层是**数组**，数组元素没有 `type` 标签；`all` tab 的元素对应 `Response`：

```json
[
  {
    "search_results": [
      {
        "result": {
          "url": "https://example.com/",
          "title": "Example",
          "description": "..."
        },
        "engines": ["google", "brave"],
        "score": 1.75
      }
    ],
    "featured_snippet": null,
    "answer": null,
    "infobox": null
  }
]
```

上述示例是依据 `Serialize` struct 形状整理的示意（不是运行时抓到的样例）；确切字段为：[S9][S12]

- 顶层 `Response`：`search_results`、`featured_snippet`、`answer`、`infobox`。`config` 字段标记 `#[serde(skip)]`，不会出现在 JSON。[S9]
- 每个 `search_results[]` 项：`result`、`engines`、`score`。[S9]
- `result`：`url`、`title`、`description`，三者在 `EngineSearchResult` 中都是必需的 Rust `String` 字段。[S9]
- `engines`：贡献结果的引擎 ID 集合（`BTreeSet<Engine>` 序列化为字符串数组）。[S9]
- `score`：排序使用的浮点总分，不是上游可信度或概率。[S11]
- `featured_snippet`（若存在）：`url`、`title`、`description`、`engine`。[S9]
- `answer`（若存在）：`html`、`engine`；`infobox`（若存在）：`html`、`engine`。[S9]

`images` tab 的元素是 `ImagesResponse`，字段为 `image_results[]`，其中每项同样带 `result`/`engines`/`score`，而 `result` 是 `image_url`、`page_url`、`title`、`width`、`height`；它不是 Stravia 的普通 Web Search 契约。[S9]

README 明确不保证 API 结构稳定，因为它依赖 internal structs 序列化，未来可能无预警变化。[S1] 因此 adapter 应拒绝未知/缺失核心字段，记录 schema version/commit，并用 `search_results[].result.{url,title,description}` 作为唯一映射路径；不要依赖 `featured_snippet`、`answer`、`infobox` 的 UI 字段。

### 5.3 映射到 Stravia `SearchResponse`

Stravia 当前公共 seam 在 `backend/crates/stravia-core/src/web_access/mod.rs`：`SearchResponse` 包含 `mode`、`query`、`results`，每项 `SearchResult` 是 `url`、可选 `title`、可选 `snippet`；`WebProviderAdapter` 要求同时实现 `supports_search`、`supports_fetch`、`search` 和 `fetch`。[S20]

建议的 search-only 映射：

| metasearch2 JSON | Stravia 字段 | 处理 |
|---|---|---|
| `search_results[].result.url` | `SearchResult.url` | 必须是可解析 URL；无效值应丢弃/按 provider failure 处理 |
| `search_results[].result.title` | `SearchResult.title` | 直接映射为 `Some(String)`；上游当前字段必有，但 Stravia 仍保留可选语义 |
| `search_results[].result.description` | `SearchResult.snippet` | 直接映射为 `Some(String)`；必要时先做 HTML/标记清理（Yep 已在引擎内清理 snippet） |
| `engines[]`、`score` | 无直接 core 字段 | 可写入 adapter 私有 telemetry/provenance；不能替代 Stravia 的 `Citation` |
| 顶层数组 | 一次 `SearchResponse` | 通常取 `all` tab 的第一个元素；必须拒绝缺失/多种不符合预期的形状，避免静默误读 |
| `featured_snippet`/`answer`/`infobox` | 无直接字段 | 不映射；只把普通 `search_results` 作为 Web Search 结果 |

`mode` 应明确为 `Index`（metasearch2 只提供聚合搜索，不提供 Stravia 所需 agentic fetch 语义），`query` 使用请求 query，`results` 可在 adapter 内按 Stravia `max_results` 截断。Stravia 的 allowed/blocked domain 过滤会在 provider 返回后再次执行；metasearch2 自身没有对应的 Web Access 输入字段，不能把 Stravia 域过滤假定为上游抓取过滤。[S20]

### 5.4 `fetch` 缺口与部署边界

metasearch2 当前 web routes 只有 `/search`、`/autocomplete`、`/settings`、`/opensearch.xml`、`/image-proxy` 等，没有正文 fetch/reader endpoint；其搜索结果链接仍指向外部 URL。[S19] `image-proxy` 是图片下载代理，不是通用正文 reader，也有服务器发起任意 URL 请求的安全含义；图片搜索默认关闭，配置注释说明启用 proxy 可能让服务器对任意 URL 发 GET。[S8]

所以一个 Stravia adapter 不能仅通过 metasearch2 满足现有 `WebProviderAdapter`：

1. 选项 A：新增只实现 search 的 metasearch adapter，同时让既有 fetch provider 负责 `web_fetch`；这需要 core 支持把 search 与 fetch provider 解耦（当前 trait 对每个 adapter 都要求两个方法）。
2. 选项 B：adapter 自己对结果 URL 实现受 Stravia SSRF/内容大小/超时策略约束的 fetch；这实际上是 Stravia 的第二套 fetch 实现，不是 metasearch2 提供的能力。
3. 选项 C：不接入，继续使用已有 Exa/Brave/Tavily/Zhipu，其中现有 `AdapterConfig` 与 `WebProviderAdapter` 已包含 search/fetch 能力。[S20]

自托管只把第一跳变为 `Stravia -> metasearch2`；源码硬编码的后续跳仍是 `metasearch2 -> www.google.com / www.bing.com / search.brave.com / ...`。因此查询内容、IP/网络元数据和访问时间仍可能出境到这些上游；自托管不是“本地索引”或“无第三方数据披露”。[S9][S10]

## 6. 配置与部署

### 6.1 配置文件位置与主要键

仓库根目录文件清单没有单独的 `CONFIG.md`；README 指向 `src/config.rs`，默认可复制配置位于 `config-default.toml`。[S1][S8] 运行形式为 `metasearch [config_file]`；不传路径时依次检查：`$XDG_CONFIG_HOME/metasearch/config.toml`、`$HOME/.config/metasearch/config.toml`、当前目录 `./config.toml`。缺少文件时会在第一个有效位置创建默认配置。[S1][S19]

关键默认值来自 `Config::default()` 和 `config-default.toml`：[S8]

```toml
bind = "0.0.0.0:28019"
api = false

[image_search]
# enabled = true

[engines]
# numbat = false
# fend = true
```

引擎配置支持三种实质语义：对已知引擎设置布尔值启用/禁用；设置表覆盖 `enabled`、`weight`；其余键进入该引擎的 `extra` TOML 表，在请求时由具体引擎解释。[S8] 默认引擎权重见第 4.2 节。`ui.show_autocomplete` 默认 true；`image_search.enabled` 默认 false，`image_search.proxy.enabled` 默认 true 且最大下载大小为 10,000,000 bytes，但图片 tab 本身默认不启用。[S8]

源码没有暴露可配置的 per-engine timeout；全局 `CLIENT` timeout 固定为 10 秒，整个 Web Access 的 60 秒 deadline 是 Stravia 自己的限制，不应混淆。[S9][S20]

### 6.2 安装与容器

README 推荐：

```sh
cargo install metasearch
cargo install --git https://github.com/mat-1/metasearch2
```

第一条安装 crates.io 发布包，第二条安装仓库当前不稳定版本；启动命令为 `metasearch [config_file]`。[S1] 仓库没有 GitHub binary release（releases 为空，`has_downloads=false`），所以“二进制部署”意味着自行 Cargo build/cargo install 后运行，不是下载官方预编译物。[S2][S4]

仓库 `Containerfile` 使用 `rust:slim-bookworm` builder 编译 release binary，再复制到 `gcr.io/distroless/cc-debian12`；runtime 暴露 `28019`，entrypoint 为 `/usr/local/bin/metasearch`。[S13] `compose.yml` 示例采用 host networking 与 `restart: unless-stopped`，并通过 build arg `CONFIG` 指向配置路径；它没有提供 API 认证或上游代理策略。[S13]

## 7. License

仓库 `LICENSE` 标题为 **CC0 1.0 Universal**；`Cargo.toml` 与 GitHub metadata 也分别标为 `CC0-1.0`。[S2][S7][S14] CC0 主要解决 metasearch2 自身代码的再使用许可；LICENSE 同时声明作品按“as-is”提供，并不负责清理第三方权利/许可。[S14]

因此 CC0 **不等于** Google、Bing、Brave、Marginalia、Stract 或 Yep 的页面/接口许可，也不替 Stravia 取得抓取、再分发结果或商用所需的上游同意。ToS、robots、版权、数据库权利和适用法需要单独进行法律审查；本文不是法律意见。[S10][S14]

## 8. 与 Stravia Web Access 的适配对照

| 维度 | metasearch2 | Stravia 当前 seam | 适配判断 |
|---|---|---|---|
| 进程边界 | 独立 Axum HTTP server，默认 `0.0.0.0:28019` | `stravia-core` 内的 `WebProviderAdapter` | 以 HTTP provider adapter 接入，不链接 crate |
| search 输入 | `GET /search?q=...`，API 需显式开启 | `SearchRequest { query, max_results, allowed_domains, blocked_domains }` | query/max_results 可映射；域过滤由 Stravia post-filter |
| search 输出 | 顶层数组，`search_results[].result.{url,title,description}`，附 `engines`/`score` | `SearchResponse { mode, query, results[] }` | 字段映射直接，但需处理不稳定 envelope、数组/untagged shape |
| fetch | 无正文 reader；只有 image proxy | `WebProviderAdapter::fetch` 与 `FetchResult` | 不能单独满足当前 trait；需要分离 seam 或第二 fetch 实现 |
| 上游类型 | 主要是无 key HTML scraping，Yep 是 JSON endpoint | 当前 providers 是 Exa/Brave/Tavily/Zhipu，均有 API-key SaaS 配置 | 信任模型从供应商 API 合约变为自建服务 + 多个被抓上游 |
| 认证 | metasearch2 API 只有 `api=true` 开关，源码无 provider auth | 现有 Stravia provider 由 `AdapterConfig` secret 管理 | 必须把 metasearch endpoint 放在私网/反代认证后，不能裸露公网 |
| 超时/失败 | 每个上游 HTTP 10 秒；单引擎失败可被忽略 | Web Access 总 deadline 60 秒，失败码含 Timeout/RateLimited/Unavailable | adapter 需把 403、超时、非 JSON、空结果和上游质量异常归一为失败/遥测 |
| provenance | 只有内部 `engines` 与 `score`，无标准 citations | `Citation` 可选，结果 URL/title 是 core 字段 | 可保存引擎 ID 为私有 metadata，但不能宣称为独立来源证明 |
| 结果质量 | 加权位置融合；受 HTML selector/上游反滥用影响 | provider adapter 期望可审计、可回退结果 | 只能 experimental/可回退，不能承诺 SaaS 级稳定性 |

最小可行 adapter 流程是：构造 `GET /search?q=...&format=json`（或严格设置 `Accept: application/json`），验证 HTTP status 和顶层数组，取 `all` tab 的 `search_results`，将 `result.url/title/description` 映射到 Stravia，应用 `max_results` 及本地域过滤，并记录 metasearch2 commit、上游引擎列表、延迟、结果数和解析失败。若当前 `WebProviderAdapter` 仍强制 fetch 方法，adapter 必须显式返回 `Unsupported`，而不是伪装出 fetch 成功。[S12][S20]

## 9. 风险矩阵

| 风险 | 一手证据 | 严重性 | 对 Stravia 的含义/缓解 |
|---|---|---:|---|
| 上游 ToS、抓取与再分发责任 | 当前引擎源码直接请求 Google/Bing/Brave 等 HTML 页面；CC0 LICENSE 不负责第三方权利 | 高 | 上线前逐一审查上游 ToS、robots/许可和地区法律；保留人工批准的 engine allowlist；不要把 CC0 当上游授权。[S10][S14] |
| CAPTCHA、rate limit、IP 声誉 | README 警告公共 demo 会被 Google 等 rate-limit；issue #33 为降低 rate-limit 机会请求关闭 autocomplete；源码未发现 CAPTCHA 解题或稳定的 backoff/circuit breaker | 高 | 不把它当无配额服务；限流、缓存（在 Stravia/反代侧）、并发上限、熔断和监控必须由部署方提供；尊重上游限制。[S1][S9][S18] |
| HTML selector 脆弱性 | 多个引擎使用 `scraper` selector；issue #24 报 Bing 无关结果，issue #46 报 Google/Bing 图片 parser 失败，PR #50 尚未合并 | 高 | 每次升级/上线做真实查询 contract probe；按引擎统计空结果、重复率和异常域名；一项引擎故障时自动禁用或回退。[S10][S15][S17] |
| Google 失效信号与发布滞后 | open PR #31 声称 Google HTML 不再工作；发布包最新 2025-07-06，而 master 仍有 2026 commit；无 GitHub release/tag | 高 | 固定 commit 并自行构建；不要只 pin crates.io `0.2.4`；维护升级 fork/补丁的预算。[S3][S4][S5][S16] |
| 查询仍会出境 | 源码 URL 硬编码为外部搜索站点；本地 metasearch server 只是中转/抓取层 | 高（隐私） | 把 metasearch 所在主机视为有外网数据出口；配置 egress firewall/proxy/DNS policy；向用户披露上游列表与日志政策。[S9][S10] |
| 无 fetch/reader | web routes 没有正文 fetch endpoint，`image-proxy` 不是 reader | 高（功能） | 不把 search backend 宣称为完整 Web Access；保留现有 fetch provider 或先改 core seam。[S19][S20] |
| API 默认关闭且无认证 | `api=false`；开启后 handler 只按 header/`format=json` 分流，源码无 auth | 高（部署） | 反代认证、私网监听/ACL、只允许 Stravia host；默认拒绝公网暴露。[S8][S12][S19] |
| API 结构不稳定 | README 明确依赖 internal structs，可无预警改变 | 中高 | adapter 严格 schema validation、commit pin、探针与回退；不要直接让模型消费上游 JSON。[S1][S12] |
| 无缓存/重复出站 | 请求 fan-out 即时抓取；源码未发现缓存层 | 中高 | 在受控边界加入短 TTL cache（同时评估隐私/ToS）；按 query、引擎和状态监控出站量。[S9][S11] |
| 服务器网络/SSRF 面 | bind 默认 `0.0.0.0`；image proxy 配置注释承认启用后可对任意 URL GET；最新 commit 专门改 image proxy SSRF 防护 | 中高 | image search/proxy 保持关闭，除非另做 URL/IP 防护；反代与容器网络隔离；不把 image proxy 当 fetch。[S3][S8][S19] |
| 搜索质量与排序语义 | 自定义 `1/(position+1)` × weight 合并；`score` 不是可信度 | 中 | 在 Stravia 只把 score 当 provider telemetry；增加结果质量采样和来源多样性指标，不把排序分数变成 citation 置信度。[S11] |

## 10. 最终建议

**决策：不作为 Stravia Web Access 默认生产 backend；可以在满足严格前置条件后作为 opt-in search-only 实验 provider。**

采用条件：

1. 在可控网络中运行 pinned Git commit，自行构建并记录 lockfile；不要将“最新 crates.io”与“最新 master”混为同一 artifact。[S3][S4][S5][S7]
2. 只启用经过法律和可靠性评估的引擎；默认四个引擎也不是稳定 API，HTML parser 变化仍可能影响结果。[S8][S10]
3. API 仅监听私网或置于有认证的 reverse proxy 后；显式 `api = true`，并以 `/search?q=...&format=json` 做 schema probe。对非 2xx、非数组、缺失 `result.url`、全空结果和延迟超限执行失败/回退。[S12][S19]
4. 将它明确建模为 search-only：`supports_fetch=false` 的能力不能被假装成成功；正文抓取仍走 Stravia 既有受 SSRF/内容上限保护的 fetch provider，或先调整 core 的 search/fetch seam。[S19][S20]
5. 在 Stravia 侧设置出站限流、短 TTL cache（若法律/ToS允许）、熔断、结果质量告警和上游域名 egress allowlist；观察 issue #24/#31/#46 所示的失效模式。[S1][S15][S16][S17][S18]
6. 只有在实测查询质量、可接受的上游政策、隐私披露和持续维护能力均满足后，才考虑扩大启用范围；否则继续使用现有 API-key provider，并把 metasearch2 当作独立可选集成而非核心依赖。

在当前证据下，metasearch2 的优势是部署简单、CC0、无供应商 API key 且能聚合多个来源；决定性劣势是抓取脆弱性、上游 ToS/CAPTCHA/rate-limit 风险、API 非稳定契约、查询继续出境以及缺少 fetch。对 Stravia 的最佳定位是“受控的搜索结果聚合器”，不是完整的 Web Access provider，也不是本地隐私搜索索引。
