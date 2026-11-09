---
status: accepted
---

# 用 Egress Shape 描述客户端出站形态

Egress Shape（出站形态）是可按名称选择的连接层形态，包括 TLS ClientHello、ALPN、HTTP/1 协议字段位置、HTTP/2 参数及 WebSocket 握手方式。它不是供应商 Profile，也不是 User-Agent、凭据或重试配置。内置形态通过 `shape` 裸名称引用；插件自有形态放入描述符的 `egress_shapes`，通过 `plugin:<local-name>` 引用。未知名称失败，不回退到其他客户端身份；显式命名空间也使旧宿主拒绝新引用，而非悄悄使用同名内置配置。

请求字段由插件按顺序构造，宿主不补 `Accept`、`Accept-Encoding` 或 `User-Agent`。协议必需的槽位字段由宿主校验并在指定位置填值；HTTP/2 的协议限制仍须执行。连接池与 TLS 会话不得跨出站形态及其修订复用，代理和目标身份继续参与复用约束。

基准来自仅连接本地监听器、使用占位凭据的真实客户端采集；记录客户端版本和平台，不把模拟客户端的数据冒充官方样本。Codex HTTP 初始对齐范围为 Linux OpenSSL；TCP/IP 和时序指纹不在范围内。内置形态更新需要发布宿主；插件自有数据则由插件仓库维护，使用既有传输原语时只需重新构建和导入插件，无需更改 WIT。代价是宿主须在安装准入和启动恢复时验证、编译不可信声明，并将其与组件作为不可拆分的执行快照。

插件形态沿用宿主证书验证器，不得覆盖内置名称、增加证书信任或扩大网络授权。HTTP、WebSocket 及 TLS 会话的复用约束包含组件内容身份与 shape 修订，兼容更新中的认证会话和在途请求仍固定旧快照。Qoder、WorkBuddy 的专属形态及捕获迁至各自插件仓库，旧的裸名引用不保留兼容别名。

当前内置 `default`、`claude-code`、`bun`、`bun-1-3-14`、`antigravity`、`codex-http`、`codex-ws`、`command-code`、`devin`、`grok`；来源和修订记录在 `backend/crates/stravia-core/src/plugin/network/shape/data/`，采集细节及脱敏结果保存在相邻 `fixtures/`。Claude Code 和 Codex 样本记录各自官方客户端的本地调用；Bun 样本是独立 Bun fetch / WebSocket 脚本，不是 Claude Code 采集。Antigravity 的 `antigravity.json` 是本地 bootstrap 请求而非 inference；`antigravity-inference.json` 另外记录官方 CLI 的本地 inference 请求及同一 socket 的 ClientHello，其上游在响应前停止。两者不能互相冒充。

这些样本只支持其记录的平台、版本、操作和已保存字段的比较：敏感或未知扩展及随机材料会被省略或归一化；Codex WS 的扩展顺序随机。测试源码包含 ClientHello 与 fixture 的比较，但不等于已测得完整客户端行为、HTTP/2 帧、WebSocket 握手或端到端一致。算法仅声明而无实现时被选中会使握手失败；TCP/IP、时序以及其他平台均不在对齐承诺内。

本决定细化 [ADR-0071](0071-orchestrate-vendors-through-host-network-capabilities.md) 的字段所有权，宿主继续拥有授权、重试、deadline 和 failover。
