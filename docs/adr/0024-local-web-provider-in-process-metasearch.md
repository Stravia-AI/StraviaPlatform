# Use in-process vendored metasearch2 for Local Web Provider search

Stravia 将 Local Web Provider 的 Internal Web Search 做成 workspace crate `stravia-web-local`，并把 metasearch2（commit `33c0b4b330e2f0cb13161a80cd80bed9f2c3008e`）的引擎 fan-out、HTML 解析和 ranking vendor 成进程内 library，而不是跑 SearXNG/metasearch2 HTTP sidecar，也不是新的 Search Backend。这样桌面端以后不必捆绑 Python/Docker sidecar，且与 stravia-core 同进程接线。本阶段只落地 library 与 example，不替换已配置的 Exa/Brave/Tavily/Zhipu。Fetch 随后进入同一 crate。查询仍会出境到 Google/Bing/Brave 等上游。
