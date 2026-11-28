# OpenAI Codex CLI 0.172.0 对齐调研与捕获记录

调研与采集日期：2026-11-28。基线版本：`rust-v0.167.0`；目标稳定版本：`rust-v0.172.0`（2026-11-24 发布，Linux x86_64 musl 二进制 SHA-256 `a2342a8f...6be392` 校验通过）。对应实现：`backend/crates/stravia-vendor-codex/`。源码差异、隔离 loopback 实测与 Stravia 本地回归分别记录，不互相替代。

## 源码差异摘要（`rust-v0.167.0..rust-v0.172.0`）

在隔离 clone 上逐文件比对 `rust-v0.167.0`（`cb41599c`）与 `rust-v0.172.0`（`fc3036db`）的关键协议与客户端改动：

- **HTTP 响应压缩协商（`codex-rs/http-client/Cargo.toml`，commit `16280e19b`）**：`reqwest` features 新增 `"gzip"` 与 `"zstd"`（与既有的 `json`、`rustls-tls`、`rustls-tls-native-roots`、`stream` 并存）。所有经由 `codex-http-client` 的 HTTP 请求自动携带 `accept-encoding: zstd,gzip`。
- **模型目录更新（`codex-rs/models-manager/models.json`）**：新增 `gpt-6.2-sol`（`context_window: 500000`）与 `gpt-6.2-astra`（`context_window: 272000`），均为 `visibility: "list"`、`minimal_client_version: "0.166.0"`、`use_responses_lite: true`、`prefer_websockets: true`、`default_reasoning_level: "low"`、`default_verbosity: "low"`、`multi_agent_reasoning_effort: "xhigh"`；`supported_reasoning_levels` 列出 `low`、`medium`、`high`、`xhigh`、`max`、`ultra`。
- **本地推理别名解析（`codex-rs/protocol/src/openai_models/reasoning_effort.rs`、`codex-rs/core/src/client.rs`）**：`ultra` 与 `persistent` 是 CLI 本地别名，发送 `/responses` 前由 `ModelInfo::resolve_reasoning_effort` 归一化——`ultra` 依次回落到受支持的 `multi_agent_reasoning_effort`、`max`、最后一个非 `ultra` 档位、`medium`；`persistent` 映射为 `"disabled"`，从不将原始 `"ultra"` 或 `"persistent"` 发送到线上 wire。
- **Responses Lite 托管 `web_search` 位置（`codex-rs/core/src/session/turn.rs`、`session/world_state.rs`、`compact_remote_v2_attempt.rs`、`scenarios_incremental_tools.rs`）**：启用增量工具 / Responses Lite 时，托管 `ToolSpec::WebSearch` 必须保留在顶层 `tools` 中，不得放入 `input` 的 `additional_tools` 声明；非 `web_search` 工具（`function`、`custom`、`namespace`、`tool_search`）仍进入 `additional_tools`。
- **请求体封闭字段契约（`codex-rs/codex-api/src/common.rs`）**：`ResponsesApiRequest` 与 `ResponseCreateWsRequest` 在声明顺序上新增 `prompt_cache_options`（位于 `prompt_cache_key` 与 `text` 之间，仅实验开关开启时发送）与 `request_country`（位于 `access_programs` 之后，默认省略）；默认请求均不携带这两项。
- **未变更或实验特性**：`x-codex-beta-features` 默认仍仅无条件广播 `remote_compaction_v2`；WS 握手头顺序、`openai-beta: responses_websockets=2026-02-06`、`sec-websocket-extensions: permessage-deflate; client_max_window_bits`、`/backend-api/codex/models`、`/backend-api/wham/usage`、`/backend-api/wham/rate-limit-reset-credits`、OAuth 端点及独立 Remote Compaction V2（末尾 `compaction_trigger`）路径均无破坏性变化。

## 隔离原生客户端实测

在 Linux user + mount + network namespace（仅 loopback，隐藏宿主机 `/run/nscd`，全新 `HOME`/`CODEX_HOME`/`XDG`/空工作目录）中运行官方 `codex 0.172.0` 交互式 TUI，仅使用本地合成占位 `auth.json` 与临时 CA（通过 mount namespace bind-mount 覆盖系统 CA 束，不设置 `SSL_CERT_FILE` / `CODEX_CA_CERTIFICATE` 以保持原生 TLS 栈），未访问任何生产接口。分别对 `gpt-6-astra` 完成两轮独立 WS 与 HTTP 回退采集，并对 `gpt-6.2-sol` 完成一轮 WS 与 HTTP 回退对照采集。

脱敏夹具保存在：

- [HTTP Round 1](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/codex-http.json) / [HTTP Round 2](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/codex-http.repeat.json)
- [WS Round 1](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/codex-ws.json) / [WS Round 2](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/codex-ws.repeat.json)

实测结果：

|观测项|HTTP 回退（`POST /backend-api/codex/responses`）|WebSocket（`GET /backend-api/codex/responses`）|
|---|---|---|
|TLS 栈与 JA4|OpenSSL，`t13d301100_1d37bd780c83_8e6e362c5eac`（与 0.167.0 逐项一致）|rustls 随机扩展序，`t13d101000_61a7ad8aa9b6_0d308c48d2a3`（与 0.167.0 逐项一致）|
|`version` / `originator`|`0.172.0` / `codex-tui`|`0.172.0` / `codex-tui`|
|`user-agent`|`codex-tui/0.172.0 (NixOS 26.5.0; x86_64) xterm-256color (codex-tui; 0.172.0)`|同左|
|相对 0.167.0 新增请求头|`accept-encoding: zstd,gzip`（紧跟 `user-agent`，位于 `host` 与 `content-length` 之前）|无（19 个握手头名称与顺序完全一致）|
|`reasoning`（Lite）|`{"effort":"low","context":"all_turns"}`|同左（`gpt-6-astra` 与 `gpt-6.2-sol` 均一致）|
|`prompt_cache_options` / `request_country` / `access_programs`|均省略|均省略|

同轮采集也观测到 `/backend-api/codex/models?client_version=0.172.0`、`/backend-api/wham/usage` 与 `/backend-api/wham/rate-limit-reset-credits` 均在 `accept: */*` 后新增 `accept-encoding: zstd,gzip`。

## Stravia 对齐改动与范围说明

- **`codex-http` / `codex-ws` Shape 修订保持 `revision: 1`**：实测两个通道的 ClientHello 密码套件、扩展集、签名算法、密钥共享组及 ALPN 与 0.167.0 完全一致，仅刷新 `source` 捕获元信息与四份脱敏夹具，`released.json` 摘要保持不变。
- **插件协议对齐（`stravia-vendor-codex`）**：
  - `CLIENT_VERSION` 升级至 `"0.172.0"`。
  - HTTP 请求头与宿主传输选项统一发送 `accept-encoding: zstd,gzip` 并启用宿主 `zstd`/`gzip` 透明解压（推理、模型发现、额度读取/重置、搜索、生图、独立压缩与 OAuth token 交换），推理路径不再剥离 `accept-encoding`。
  - 模型发现过滤非 wire 的本地 CLI 别名 `ultra` / `persistent`（保留原始 `supported_reasoning_levels` 与 `multi_agent_reasoning_effort` 元数据），请求投影按官方 `resolve_reasoning_effort` 规则归一化后写入请求体与 `turn_metadata`。
  - Responses Lite 投影将托管 `web_search` 保留在顶层 `tools`，仅将其余工具声明移入 `input` 的 `additional_tools`。
  - `WIRE_FIELDS` 与 `SESSION_FIELDS` 同步加入 `prompt_cache_options` 与 `request_country`，保持调用方不可透传且默认省略。
- **验证边界**：本轮全部捕获与验收在本地隔离 loopback 环境完成，未调用真实 OpenAI / ChatGPT 生产服务，不验证生产账号资格、真实上游限流策略、实验性 `prompt_cache_options` / `request_country` / connector tool results 注入或非 Linux 平台行为。
