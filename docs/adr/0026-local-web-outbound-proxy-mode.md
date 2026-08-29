# Give Local Web one snapshotted outbound proxy mode

`stravia-web-local` 用构造期快照的 **Local Web Outbound Proxy Mode**（Direct / System / Explicit）覆盖 Internal Web Search、Static Extraction 和 Rendered Extraction（含 Chrome 子资源）。System 只认进程环境变量，不认 OS GUI/PAC/WinHTTP；有可用代理时源站 DNS 交给代理，Chrome 禁止本机源站解析。这样在必须走代理才能出网的环境里，三条路径不会各走各的出口。

## Considered options

- wreq 读环境变量、Chrome 跟 OS 默认：两条出站不是同一代理。
- 代理必填：弄坏 fixture 测试和现有 example。
- 有代理仍本机解析并钉死源站 IP：真正需要代理的环境会失败，或绕过代理直连。
- 现在就把 Gateway `proxy_url` 接进来：把模型上游代理和 Local Web 出站缠在一起。

## Consequences

- Library 入口是一次构造的运行时，而不是进程级 `init()` 或每请求代理参数；本阶段只改 crate 与 example。
- System 按 scheme 快照 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`（大写优先）和 `NO_PROXY`，再把同一快照配给 HTTP client 与 Chrome；Explicit 忽略环境变量；非法 URL 或带 userinfo 的代理在构造时失败，不退化成直连。
- Explicit 是一个 URL，允许 `http`/`https`/`socks5`/`socks5h`；`socks5h` 交给 Chrome 时写成 `socks5`（Chrome 的 SOCKS5 已是远端 DNS）。
