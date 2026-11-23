# Web Access Provider

Web Access 提供基础搜索与公开网页读取。宿主通过 Rust 工厂接入搜索服务、抓取实现或 Worker，无需修改 Core。
Web Provider 不属于 Wasm Vendor Plugin 的推理适配面，也不负责综合搜索报告。

开源 Core 默认注册表为空，不内置 LocalWeb、Chromium 或第三方 Web Provider。[External Search Route](web-search.md#external-route) 不依赖该注册表。
接入决策见 [ADR-0002](../adr/0002-web-access-provider-seam.md)。

## 模块与装配

| 模块 | 职责 |
| --- | --- |
| [stravia-web-access-contract](../../backend/crates/stravia-web-access-contract/) | 定义工厂、适配器、注册表、描述符、配置和出站模式；拥有静态地址策略，不创建网络客户端。 |
| [stravia-web-access](../../backend/crates/stravia-web-access/) | 提供公开地址重定向解析、正文解码、HTML 提取、远程 MCP Streamable HTTP 与显式 Wire Debug 捕获；重导出契约。 |
| [stravia-core](../../backend/crates/stravia-core/) | 装配工厂，管理记录、来源顺序、代理快照与运行时执行。 |
| Server / Desktop | 共用 Core 管理规则；管理面根据类型描述符和脱敏视图展示配置。 |

宿主实现 [WebProviderFactory](../../backend/crates/stravia-web-access-contract/src/factory.rs)，通过 `GatewayBuilder::web_provider_factory` 注册。
构建时校验并冻结注册表，Admin API 不安装执行代码。浏览器或 Worker 依赖留在宿主实现，不进入 Core。

## 类型、配置与可用性

- `descriptor` 的 `kind` 是稳定非空的小写标识，注册表拒绝非法或重复标识。
  描述符声明 `search`、`fetch`、`requires_api_key`、`singleton`、`allow_delete`。
- 持久化配置可为空；非空须为 JSON 对象。凭据与私有配置不属于描述符。
  创建未传配置时使用 `default_config`。
- 更新可用 `merge_config` 保留未修改的私有字段；默认整对象替换。
  `validate_config` 校验密钥与配置，允许规范化配置，Core 随后复查对象形状。
- 默认要求需要密钥的类型具有非空密钥，不接受向无密钥的类型显式提交密钥。
- `is_available` 表示工厂当前运行条件是否满足，缺省为真。
  未就绪时由 `unavailable_reason` 提供可公开的错误码和说明。
- `build` 用本次记录与 `OutboundProxyMode` 构造用于连接测试或执行的适配器，不自行读取进程全局代理来覆盖宿主决定。

注册成功、记录配置有效、最近一次连接测试成功与当前可执行，是四个不同事实。

## 来源选择与代理

`WebAccessSettings` 分别保存有序的 `search_provider_ids` 与 `fetch_provider_ids`。
默认两表为空，Web Access 无独立总开关；完整能力可用性见 [Web Search](web-search.md)。

1. 每次执行取得当前设置、Provider 记录和共享出站代理快照，按能力选择对应列表。
2. 按优先级跳过缺失记录、未注册类型、不支持该能力或当前不可用的工厂，不让单个离线来源阻塞其余来源。
3. 根据记录的 `use_proxy` 与[共享出站代理配置](architecture.md#共享出站代理配置)构造适配器。
4. 在同一个 `WEB_ACCESS_DEADLINE` 内按顺序切换。批量读取只把失败的 URL 交给下一个来源，成功项不重抓。

`use_proxy=false` 使用 `Direct`；为真时必须配置合法的共享代理地址，使用 `Explicit { url, bypass }`。
解析失败不回退直连。工厂及实际发起请求的 Worker 均须遵守该次出站快照。

设置保存拒绝重复 ID、缺失记录、未注册类型和能力不匹配项。新加入的来源必须当前可用；
已选且仍满足类型和能力约束的来源可以暂时离线，管理员仍可移除它或调整其他来源。

## 管理接口

以下端点均要求管理员鉴权：

| 方法与路径 | 行为 |
| --- | --- |
| `GET /api/v1/web-provider-kinds` | 枚举类型描述符；开源默认返回空数组。 |
| `GET /api/v1/web-providers`、`GET /api/v1/web-providers/{id}` | 列表与详情使用 `WebProviderView`。 |
| `POST /api/v1/web-providers` | 提交 `name`、`kind` 与可选 `api_key`、`use_proxy`、`config`；成功返回 201 和视图。 |
| `PUT /api/v1/web-providers/{id}` | 修改名称、凭据、代理偏好或配置并返回视图，不改变 `kind`。 |
| `DELETE /api/v1/web-providers/{id}` | 成功返回 204，并原子移除两份来源列表中的引用。 |
| `POST /api/v1/web-providers/{id}/test` | 经已注册工厂测试连接，记录测试状态；未注册或不可用不报告成功。 |
| `GET / PUT /api/v1/web-access/settings` | 读取或替换优先级；校验失败返回结构化错误。 |

仅能创建、编辑已注册类型；名称去除首尾空白后保持唯一。更新时省略 `api_key` 或 `config` 保留原值；
显式 `null` 请求清除，空白密钥按清除处理，结果仍须通过类型校验。

| 错误码（HTTP 400） | 原因 |
| --- | --- |
| `WEB_PROVIDER_NAME_CONFLICT` | 名称重复。 |
| `WEB_PROVIDER_SINGLETON_CONFLICT` | 单例类型已存在记录。 |
| `WEB_PROVIDER_DELETE_FORBIDDEN` | 已注册类型禁止删除。 |
| `WEB_PROVIDER_UNAVAILABLE` 或工厂自定义码 | 新选中的来源当前不可用。 |

### 脱敏视图

`WebProviderView` 返回 `api_key_configured`，不返回凭据。`config` 只允许来自工厂的 `public_config_view`，默认省略。
该投影必须删除 Cookie、Token 等全部私密值，包括嵌套字段；不得直接返回原配置。

视图包含注册、可用、能力、可删除状态与最近连接测试结果。未注册记录显示 `registered=false`、`available=false`、`capabilities=null`、`allow_delete=true`，且不返回配置。

管理面区分请求失败、空注册表、未安装与已注册但暂时不可用。未安装记录可查看状态及删除，不可编辑、测试或再次加入优先级；
已有引用可取消。表单按描述符显示密钥输入和单例限制，已注册记录按 `allow_delete` 删除。

## 公网读取安全

[静态地址策略](../../backend/crates/stravia-web-access-contract/src/address_policy.rs)只允许无嵌入凭据的公网 HTTP(S) 地址，拒绝本地名称、私网、环回、链路本地和保留地址等；
静态通过不等同于 DNS 或连接校验通过。

- 执行初次请求与每次重定向都检查目标；直连拒绝空、失败或包含非公网地址的解析结果，固定连接地址并复核对端，限制重定向次数。
- 显式代理的源地址解析由代理承担；不为代理请求新增 origin DNS，也不宣称已执行本地地址固定。
  按绕过规则走直连时仍执行直连校验。
- 外部适配器负责实际请求、重定向和子资源出口；Core 的入口和结果校验不能替代实际连接检查。
- 公网读取受响应大小、时限和取消约束。网页及提取文本是不可信数据，不可改变工具权限或系统指令。

`resolve_final_url` 用于只需确定最终地址的场景，不读取正文；`convert_read_bytes` 提供严格解码与有损阅读转换，`resource` 提供 HTML 正文提取。
远程 MCP 使用独立 HTTP 传输及有界事件读取。Wire Debug 由调用方显式传入捕获器，见[交互观测](interaction-observation.md#wire-debug-与脱敏边界)。

## 宿主接入顺序

1. 启动时注册类型工厂，不在启动代码里偷偷写入管理员的来源顺序。
2. 管理员先保存 Provider 记录，可选择测试连接；测试不会自动将它加入搜索或读取列表。
3. 按工厂声明将记录加入一张或两张来源表，并分别确定顺序。
4. Local Agent 需要两种来源；只有搜索能力的工厂可与独立读取来源组合。

宿主不支持某项基础能力时，在描述符中省略该能力资格，不能用空结果伪装成功。
工厂对 host/worker 配置的敏感内容负责，连接测试的公开结果与可用原因同样不得带真实密钥。

数据库约束见 [SQLite](../database/sqlite.sql) 与 [PostgreSQL](../database/postgres.sql)。
