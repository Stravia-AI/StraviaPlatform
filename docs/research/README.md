# 上游调研与实证记录

本目录**只保留**无法被源码或设计文档替代的一手外部事实：私有供应商协议/CLI 抓包证据、厂商行为限制核实，以及端到端基准测试记录。

- 当前产品架构与模块契约见 [`docs/design/`](../design/README.md)
- 架构决策与取舍见 [`docs/adr/`](../adr/README.md)
- 领域名词定义见 [`GLOSSARY.md`](../../GLOSSARY.md)

> 本目录中的抓包与实测仅代表对应采样版本和截点的历史观测，不构成当前版本的兼容性承诺。

## 保留文档索引

### 供应商私有协议与 CLI 实证

| 文档 | 对应插件 | 记录内容 |
| --- | --- | --- |
| [antigravity-oauth.md](antigravity-oauth.md) | `stravia-vendor-antigravity` | OAuth、项目准入、额度与线协议形态（含 proto3 零值与缺失区分） |
| [antigravity-model-mapping.md](antigravity-model-mapping.md) | `stravia-vendor-antigravity` | Agent 推理模型目录筛选与 family/effort 合并证据 |
| [antigravity-cli-web-search.md](antigravity-cli-web-search.md) | `stravia-vendor-antigravity` | 独立搜索操作、grounding 与来源校验实证 |
| [claude-code-fable-5-2.md](claude-code-fable-5-2.md) | `stravia-vendor-claudecode` | Claude Code CLI 原生与 Stravia 的思考/effort/beta 对照 |
| [codex-cli-0-172-alignment.md](codex-cli-0-172-alignment.md) | `stravia-vendor-codex` | Codex CLI `0.172.0` 传输、Responses Lite 与推理档位对齐实证 |
| [devin-cli-3000-15-14-alignment.md](devin-cli-3000-15-14-alignment.md) | `stravia-vendor-devin` | Devin CLI `3000.15.14` H2/Connect gzip 抓包与 `GetCliModelConfigs` 元数据实证 |
| [grok-oauth.md](grok-oauth.md) | `stravia-vendor-grok` | Grok OAuth Device Flow 上游对照与信任边界（注：文中早期架构设想为历史记录） |
| [kiro-cli-request-alignment.md](kiro-cli-request-alignment.md) | `stravia-vendor-kiro` | Kiro CLI TUI/ACP/SDK 请求线协议对照与版本差异 |
| [kiro-cli-credits.md](kiro-cli-credits.md) | `stravia-vendor-kiro` | `GetUsageLimits` 额度字段语义与真实响应证据 |
| [kiro-cli-tls.md](kiro-cli-tls.md) | `stravia-vendor-kiro` | Kiro CLI TLS ClientHello / ALPN 出站形态抓包来源 |
| [opencode-free-models.md](opencode-free-models.md) | `stravia-vendor-opencode-free` | OpenCode Zen 免费模型受控对照、失效假设与使用限制 |

### 协议能力与基准实证

| 文档 | 关联范围 | 记录内容 |
| --- | --- | --- |
| [upstream-remote-compaction.md](upstream-remote-compaction.md) | Responses 原生压缩 | 多厂商原生压缩机制一手对照与 Codex 端点实证（Stravia 已按 [ADR-0098](../adr/0098-remove-anthropic-remote-compaction.md) 仅保留 Responses 压缩） |
| [protocol-http-benchmark.md](protocol-http-benchmark.md) | 存储与四协议性能 | 四协议真实 HTTP 端到端基准方法、历史矩阵与 ADR-0089 实测脚本口径 |
