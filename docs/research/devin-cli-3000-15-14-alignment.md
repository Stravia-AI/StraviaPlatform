# Devin CLI `3000.15.14` 协议对齐与模型元数据实证

- 采样日期：`2026-11-28`
- 采样目标：官方 Devin CLI（`chisel`）`3000.15.14`（commit `cfa79668e1a5`，Linux x86_64 压缩包 SHA-256 `d219f885…13f28`）
- 历史基线：`3000.11.3`（`shape/data/devin.json` `revision: 1`）
- 对应代码：`backend/crates/stravia-vendor-devin/`、`backend/crates/stravia-core/src/plugin/network/shape/data/devin.json`、`backend/crates/stravia-core/src/plugin/network/shape/fixtures/devin-interactive*.json`

## 1. 传输层与出站形态（TLS / HTTP/2 / Connect-RPC）

### 1.1 TLS 与 HTTP/2 协商变化（`shape/data/devin.json` `revision: 1 -> 2`）

旧版 `devin.json`（`revision: 1`）在仅允许 HTTP/1.1 的捕获环境中录制，记录了 `alpn.http = []`、`key_shares = ["X25519MLKEM768", "X25519"]` 与空的 `http2` 配置。使用支持 ALPN `h2` 的隔离本地 TLS 服务器重新捕获 Devin CLI `3000.15.14` 交互式冷启动（无 TLS ticket）与复用连接（启用 ticket + GOAWAY 重连）各两组独立样本后，实测如下：

| 维度 | 冷握手（`devin-interactive{,.repeat}.json`） | 票据复用（`devin-interactive.resumed{,.repeat}.json`） |
| --- | --- | --- |
| ALPN offer / negotiated | `["h2", "http/1.1"]` / `h2` | `["h2", "http/1.1"]` / `h2` |
| JA4 | `t13d1011h2_61a7ad8aa9b6_f9531d972513` | `t13d1011h2_61a7ad8aa9b6_35d235a1c490` |
| `supported_groups` | `[29, 23, 24, 4588]` (`X25519`, `secp256r1`, `secp384r1`, `X25519MLKEM768`) | `[29, 23, 24, 4588]` |
| `key_share_groups` | `[29]` (`X25519`) | `[4588, 29]` (`X25519MLKEM768`, `X25519`) |
| PSK extension `41` | 无 | 有 |
| HTTP/2 `SETTINGS` | `[[2, 0], [4, 2097152], [5, 16384], [6, 16384]]` | 同左 |
| HTTP/2 初始 `WINDOW_UPDATE` | `stream_id = 0`, `increment = 5177345` | 同左 |
| 伪头顺序 | `:method`, `:scheme`, `:authority`, `:path` | 同左 |

线上 `https://server.codeium.com` 同样协商 `h2` / TLS 1.3。因此 `shape/data/devin.json` 升级至 `revision: 2`，启用 `alpn.http = ["h2", "http/1.1"]`、`key_shares = ["X25519"]` 以及上述 HTTP/2 参数。

### 1.2 请求头顺序与 Connect-RPC Gzip 压缩阈值

逆向二进制压缩路径并对照实测抓包确认：

- **Connect 请求体压缩阈值**：当 `GetChatMessageRequest` 序列化后的 protobuf 字节数 `>= 8192`（`0x2000`）时，CLI 将 Connect 帧标志位设为 `0x01`（gzip 压缩），并在 `sentry-trace` 与 `content-type` 之间插入 `connect-content-encoding: gzip`；低于 `8192` 字节时保持未压缩（`flag = 0x00`）并省略该头。线上 `server.codeium.com` 实测对两种帧均返回 HTTP 200。
- **交互式 `GetChatMessage` 头部顺序（H2）**：
  1. `:method`, `:scheme`, `:authority`, `:path`
  2. `authorization: Basic <token>-<token>`
  3. `sentry-trace: <32hex>-<16hex>-1`
  4. `connect-content-encoding: gzip`（仅当 `proto.len() >= 8192`）
  5. `content-type: application/connect+proto`
  6. `connect-protocol-version: 1`
  7. `accept: */*`
  8. `accept-encoding: gzip`
  9. `content-length`
- **一元 RPC（`GetCliModelConfigs`、`AssignModel`、`GetUserStatus`）**：均携带 `authorization`、`content-type: application/proto`、`connect-protocol-version: 1`、`accept: */*`、`accept-encoding: gzip`；宿主侧同步启用 `response_decoding(&[Coding::Gzip])`。
- **`ClientMetadata`**：`field_2` 与 `field_7` 更新为 `3000.15.14`；`field_12` 与 `field_28` 仍为 `chisel`；`field_1` 在 `GetChatMessage`、`GetCliModelConfigs` 与 `AssignModel` 中均为 `devin-cli`，在 `GetUserStatus` 中为 `chisel`。`GetCliModelConfigs` 仍携带 `#30 = [3, 4, 6, 7, 8]`。
- **OAuth PKCE 常量**：`CLIENT_ID`（`3GUryQ7ldAeKEuD2obYnppsnmj58eP5u`）、`/auth/cli/continue`、`ExchangeDevinCLIPKCECode`、`chisel-show-auth-token`、`cli_pkce_marker` 均未变化。

## 2. `GetCliModelConfigs` Protobuf Schema 与线上元数据异常根因

通过逆向 `ClientModelConfig` / `ModelInfo` / `ModelFeatures` / `ModelFamilyMetadata` / `ModelDimension` 字段表并对照线上真实 `GetCliModelConfigs` 响应（带 `#30 = [3, 4, 6, 7, 8]` 共 781 条，覆盖 55 个模型家族），定位四类异常根因：

### 2.1 字段实证映射

| 路径 | 语义 | 备注 |
| --- | --- | --- |
| `ClientModelConfig.#1` | `label` | 展示标签，如 `"GLM-5.2 High"` |
| `ClientModelConfig.#4` | `disabled` | 为 `1` 表示当前账号不可调用；实测对禁用模型发起 `GetChatMessage` 返回 Connect trailer `not_found: This model is no longer available for your account.` |
| `ClientModelConfig.#5` | `supports_images` | 顶层图片支持标记 |
| `ClientModelConfig.#10` | `provider` | 上游提供商枚举：`1=cognition`, `2=openai`, `3=anthropic`, `4=google`, `5=xai`, `6=deepseek`, `7=moonshot`, `9=zhipu`, `11=nvidia`, `13=meta` |
| `ClientModelConfig.#18` | `max_tokens` | 顶层上下文窗口（禁用条目常省略） |
| `ClientModelConfig.#22` | `model_uid` | `GetChatMessageRequest.#21` 期望的 selector |
| `ClientModelConfig.#23.#4` | `ModelInfo.max_tokens` | 完整上下文窗口（可作为 `#18` 兜底） |
| `ClientModelConfig.#23.#6.#11` | `ModelFeatures.supports_images` | 除 4 个内部辅助伪条目外与顶层 `#5` 完全一致；proto3 省略 `false`，因此 `ModelFeatures` 存在且未置 `1` 时可明确判定纯文本 |
| `ClientModelConfig.#23.#6.#12` | `ModelFeatures.supports_tool_calls` | 工具调用支持 |
| `ClientModelConfig.#23.#6.#15` | `ModelFeatures.supports_thinking` | 思考能力支持 |
| `ClientModelConfig.#23.#13` | `ModelInfo.max_output_tokens` | 最大输出 token 上限 |
| `ClientModelConfig.#23.#20` | `ModelInfo.harness_uids` | `repeated string`，用于标识可用 harness（如 `fusion`），**并非**默认模型标记 |
| `ClientModelConfig.#23.#23` | `ModelInfo.model_family_uid` | 家族聚合 ID；无此字段者为内部伪条目（`subagent-default`、`session-titler` 等） |
| `ClientModelConfig.#23.#25` | `ModelInfo.is_model_router` | 为 `1` 时需先调 `AssignModel` 获取 `assignment_jwt` |
| `ClientModelConfig.#30.#1` | `ModelFamilyMetadata.model_family_label` | 官方家族展示名（如 `"Claude Opus 5"`、`"Nemotron 3.5 Lightning"`） |
| `ClientModelConfig.#30.#2` | 家族维度键值表 | `Effort` / `Reasoning Effort`（`No Thinking`/`None`/`Minimal`/`Low`/`Medium`/`High`/`XHigh`/`Max`）、`Fast Mode` / `Speed`（`order == 1`）、`1M Context`（`order == 1`）、`Thinking`（`order == 1`） |
| `ClientModelConfig.#31` | `is_default_model_in_family` | 上游权威家族默认 selector 标记 |
| `ClientModelConfig.#32` | `ModelDimension` | `#1` 维度名（`Input`/`Cached input`/`Output`）、`#2` `f32` 单价、`#3` `"1M tokens"`、`#6` `kind = 1`（价格维度） |

### 2.2 四类异常根因与修复对照

1. **大量模型元数据缺失与幽灵模型**：旧实现在 `discover()` 中无条件追加 17 条硬编码 `STATIC_MODELS`。免费层账号上游仅启用 `swe-1-6-slow`、`swe-2-flash-medium` 及若干内部伪条目，其余旗舰模型均带 `#4 disabled = 1`；硬编码追加导致 14+ 个不可调用幽灵模型（含已不存在的 `deepseek-v4`、`gemini-3-1-pro`）以 `entry = None` 注入数据库，缺失 `context_window`、价格与提供商字段。**修复**：移除 `STATIC_MODELS`，严格信任线上 `GetCliModelConfigs` 返回的启用模型。
2. **`Claude Opus 5` / `Claude Sonnet 5` 缺图片输入模态**：一方面 `STATIC_MODELS` 以 `entry = None` 注入并由旧 `discovered_model()` 强行写入 `modalities: {"input": ["text"], "output": ["text"]}`，覆盖了 Stravia 规范目录中的图片模态；另一方面解码器此前只读取顶层 `#5` 而忽略 `ModelFeatures.#11`。**修复**：合并 `#5` 与 `ModelFeatures.#11`（当两者与 `ModelFeatures` 均缺失时省略 `modalities`，避免覆盖规范目录），并移除幽灵注入。
3. **推理强度缺失（`未登记`）与无后缀档位解析失败**：旧实现仅依赖 selector 尾缀，既忽略 `#30`（`ModelFamilyMetadata`）中的 `Effort`/`Reasoning Effort`、`Fast Mode`/`Speed`、`1M Context`、`Thinking`，也未识别 `-lightning` 速度尾缀（且对 `glm-5-2`/`glm-5-2-1m` = High、`swe-1-7` = Max 这类无档位尾缀 selector 无法反向路由）。此外 `STATIC_MODELS` 对多数家族只注入 1 条 selector，导致所有家族落入 `< 2` 档位而写入空数组 `[]`。**修复**：完整解析 `#30` 与 `-lightning` 速度标志，在 `SelectorTable` 中持久化 `effort_selectors` 显式映射供 `resolve_selector` 优先查表（并保留对旧元数据的尾缀回退）；单档位/不可切换模型保持 `reasoning_efforts: []` 以 fail-closed 隐藏无效选择器。
4. **默认模型误选、Provider 枚举不全与输出上限缺失**：旧实现误把 `#23.#20`（`harness_uids`）当作默认标记；`devin_upstream_provider_name` 缺少 `5=xai`、`6=deepseek`、`11=nvidia`、`13=meta`；未解码 `#23.#4`（`ModelInfo.max_tokens` 兜底）与 `#23.#13`（`max_output_tokens`）。**修复**：改用 `#31`（`is_default_model_in_family`）驱动默认选择（在 1M 池场景同步按家族默认档位对齐），补全 10 个已实证提供商枚举，补充 `ModelInfo.max_tokens` 兜底与 `max_output_tokens` 输出。
