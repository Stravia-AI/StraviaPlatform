# Use in-process fetch with quality-gate Chromium fallback

Stravia 将 Internal Web Fetch 的本地实现放进 `stravia-web-local`，产出 **Fetched Page**（请求 URL、最终 URL、title、Markdown 主内容、实际抽取路径、limitations），而不是 Artifact，也不是站点 crawl。crate 返回完整 Markdown，只保留防失控硬顶；按字符截断、offset/窗口和给模型的剩余读取由以后的 `stravia-core` / Web Access 负责。本阶段只落地 library 与 example，不接线 `stravia-core`、不改 Web Provider schema、不新增公开工具。

管道是 HTTP 优先的 **Static Extraction**（主内容 → Markdown），用 OMP 同款质量门判定 **Low-Quality Extraction**（过短且 JS-gated，或短行导航密度过高），再对该 URL 做一次 **Rendered Extraction**（crate 已有 headless Chrome）。不预分类 SSR/SPA。两条路径都差时返回较好的一份并附 limitations。只接受公网 HTTP(S)；HTML 走抽取，markdown/plain 原样，JSON/XML 最小可读化，其余 MIME 失败；不读 `robots.txt`；不把 PDF/Office 并进 fetch。

不整段移植 OMP fetch：OMP 把 SPA 留给独立 `browser` 工具，并在 reader 链上使用 Jina/Parallel/trafilatura。Internal Web Fetch 是单一 leaf，Local Web Provider 也不使用第三方 fetch API key，所以把浏览器收进同一 leaf，远程 reader 和 70+ 站点 handler 都不进本阶段。`llms.txt` / `.md` alternate 后做。
