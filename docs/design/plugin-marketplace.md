# Vendor Plugin 市场与分发

> 状态：本仓库的实现契约。2026-11-10。
> 范围：Zstd 压缩分发、官方及第三方来源、签名验证、安装与更新管理；页面按已批准的统一工作台规格重做。
> updater 路由部署、WorkBuddy 发布迁移、生产签名密钥与 CI 配置属于外部前置条件，本仓库不代为完成。见第 10 节。

## 1. 结论与范围

市场是现有「供应商插件」管理面的在线获取入口，不是另一套插件执行或安装系统。

- 默认源为 `Stravia-AI/StraviaPlatform`；发现其中的专属插件，不单独分发内置 `base`。
- 第三方源接受 GitHub 仓库地址，例如 [WorkBuddy](https://github.com/Chikage0o0/stravia-vendor-workbuddy)。添加后自动读取 Release 及其机器可读目录，采用与官方插件相同的下载、校验、预览、确认链路。
- 发布 `.wasm.zst`，参数为 `zstd -19 --single-thread`，不使用外部字典、`--long` 或 `--ultra`。采用 Zstd 是用户选定的依赖复用取舍，不声称它压缩体积最小；实测见[压缩评估](../research/wasm-compression.md)。去函数名是独立构建决策，不作为市场接入前提。
- 自动发现与检查更新，安装和更新仍由管理员预览后确认；不静默让新代码继承连接凭据。
- updater 负责 Release 元数据和下载路由；Core 负责来源信任、完整性、兼容性与安装授权。WebUI 不从 GitHub 下载或自行解压。
- 保留本地 `.wasm` 导入；扩展 `.wasm.zst` 本地导入，使用同一受限解码器。市场只认压缩产物，不用文件名猜测旧仓库的插件身份。

非目标：用户上传到中心仓库、付费交易、评分推荐、自动代码审核、私有 GitHub 仓库、任意 URL 镜像、多格式压缩协商、插件运行时 ABI 改造。

### 现行决策与变化

[插件设计](vendor-plugins.md)中的管理范围已包含市场与远程获取，并增加自动检查更新，**不增加无确认的自动安装**。

[ADR-0091](../adr/0091-authenticate-plugin-catalogs-and-pin-install-origins.md)补充 [ADR-0068](../adr/0068-trust-vendor-plugins-with-connection-credentials.md)的来源验证与升级授权边界；签名只能验证发布来源，不能证明插件不会滥用已授权凭据。[ADR-0039](../adr/0039-pin-tauri-updater-signing-key.md)的 Desktop 公钥不是插件公钥。[ADR-0072](../adr/0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)的 Component + WIT 执行模型不变。

## 2. 实现与外部依赖

| 项目 | 本仓库行为或外部前置条件 |
| --- | --- |
| Platform Release | 生成九个专属插件的 `.wasm.zst`、v2 目录及独立签名；生产签名配置缺失时发布失败 |
| Platform Core | 来源登记、目录发现、受限下载、验签与解压、预览确认、安装来源记录；保留本地导入 |
| updater | 多仓库插件路由、资产授权及缓存隔离已在外部仓库实现，尚未部署；客户端不会把服务故障当作未收录 |
| 目录协议 | Core 使用 v2；规范和 JSON Schema 的唯一维护位置在 updater 仓库，已切换到 v2 |
| WorkBuddy | 外部维护者须用同一工具发布目录、Zstd 资产与签名；旧 Release 不自动成为市场来源 |

依据：[发布工作流](../../.github/workflows/release.yml)、[插件管理](../../backend/crates/stravia-core/src/plugin/manager.rs)、[插件类型](../../backend/crates/stravia-core/src/plugin/types.rs)、[安装存储](../../backend/crates/stravia-core/src/plugin/store.rs)、[产品更新客户端](../../backend/crates/stravia-core/src/admin/updates.rs)。外部仓库的本地调查位置见第 11 节。

## 3. 来源与安装身份

### 来源登记

一个来源对应一个公开 GitHub 仓库，至少记录：

- 内部 `source_id`、GitHub 数字 `repository_id`、当前 `owner/repo`、仓库网页地址。
- 已确认的签名公钥及其完整指纹、信任修订号 `trust_revision`。
- 是否启用、是否包含预发布；默认启用、排除预发布。
- 最近检查时间、最近成功时间与错误状态；旧目录只作带过期提示的展示缓存。

添加入口只接受 `https://github.com/{owner}/{repo}`，允许末尾 `/` 和 `.git`；拒绝 userinfo、端口、query、fragment、其他主机与任意子路径。通过 GitHub 元数据解析数字仓库 ID，而不是把可变仓库名当永久身份。仓库改名保持同一 ID；旧名字被其他仓库复用时不得接管来源。首次添加无法直接访问 GitHub 时，可经 updater 提供的仓库解析结果预览，但界面明确首次信任依赖当前网络路径。

官方来源固定为仓库 ID `1350654373`，插件目录公钥固定在 `backend/crates/stravia-core/assets/stravia-plugin-catalog.pub` 并直接编译进 debug/release 宿主，不依赖环境变量；不能由下载的目录自报为「官方」。可禁用，不能删除或替换为第三方公钥。旧构建因缺少公钥而停用的官方来源在下次启动时恢复，管理员主动禁用的来源保持禁用；公钥轮换只随宿主更新引入，不生成替代密钥。

### 安装来源

`builtin/local` 继续表示既有安装方式，增加 `marketplace`。每条市场安装记录额外保存来源 ID、仓库 ID、公钥指纹、Release ID/tag、目录摘要、压缩资产摘要和最终 Component 摘要；与安装 revision 同一事务提交。`vendor_id` 仍是运行与安装身份，不改成 `(source_id, vendor_id)`，同一 Vendor 不并行安装多个来源。

- 更新只跟随已安装来源，不从所有来源中挑最高版本覆盖它。
- 同名 Vendor 的不同来源分别展示，明确「替换来源」，经过与升级相同的影响预览。
- 市场不提供 `base` 的覆盖；现有本地替换与恢复规则不借市场扩大权限。
- 本地导入不会因字节与官方发布相同而自动获得官方来源身份；可显式从来源重新安装并确认，以建立关联。
- 删除来源不卸载插件、不清除 Provider 或凭据。安装来源快照保留并显示「来源已移除」，停止自动检查。重新添加必须重新确认信任，不按名字静默绑定。

## 4. Release 目录契约

### 资产与版本

每个可接入的 Release 发布三个公共元数据资产，以及目录列出的组件：

```text
stravia-plugin-catalog.json
stravia-plugin-catalog.json.minisig
stravia-plugin-catalog.pub
<发布者选择的安全文件名>.wasm.zst
```

目录使用 `schema_version: 2`，与 updater 现有 v1 明确区分。不得把 `.zst` 填入 v1 的 `application/wasm` 产物。协议规范和 JSON Schema 的唯一维护位置继续在 updater 仓库；Platform 发布工具固定相应协议版本，不能复制后各自演进。迁移时所有新发布者与消费者一次切到 v2；未适配的旧 Release 不猜测安装，保留手工导入途径。

目录根字段：`schema_version`、`repository_id`、`release_tag`、`generated_at`、`releases`。目录绑定仓库 ID 与 tag，不能跨仓库或 tag 复用。每个 Release 的目录只描述该 Release 实际存在的资产，可包含多个 Vendor。

| 发布记录字段 | 约束 |
| --- | --- |
| `vendor_id`、`version` | 与真实 descriptor 完全一致；version 为无 `v` 前缀的 SemVer |
| `display_name`、`description`、`authors` | 浏览信息，作者是自报信息，不是认证身份 |
| `icon_svg` | 可选，最多 32 KiB；只作图像，不作为 HTML 注入 |
| `published_at` | RFC 3339；展示时间，不用于版本排序或证明新鲜性 |
| `compatibility.host_version_req` | Rust `semver::VersionReq`；发布者显式声明，不从 SDK 版本猜测 |
| `compatibility.wit_version` | 实际 Vendor WIT ABI 版本 |
| `compatibility.canonical_format_version` | 实际 canonical 代际 |
| `artifact.filename` | 同一 Release 下唯一资产名，不是任意 URL；禁止路径分隔符、控制字符和路径逃逸 |
| `artifact.compression`、`artifact.media_type` | 分别固定为 `zstd`、`application/zstd` |
| `artifact.size`、`artifact.sha256` | 实际 `.wasm.zst` 字节数与 SHA-256 |
| `component.size`、`component.sha256` | 最终解压后 Wasm 字节数与 SHA-256 |

目录不含地域下载 URL。宿主将经签名的文件名与当前 Release 资产匹配，由下载路由决定地址；updater 不修改签名字节，也不重新签名。Release tag 不必等于插件版本，官方一个 tag 可以包含多个插件；插件版本以目录及 descriptor 一致性为准。

严格拒绝未知 schema 版本、重复 JSON 键、未知字段、同目录重复 `(vendor_id, version)`、重复资产名、无效 SemVer/范围和超限内容。多个历史目录中出现同一 Vendor/version 但不同 Component 摘要时，该来源的该版本标记冲突，不任选一个安装；字节完全相同的重复记录可合并。SemVer build metadata 不提供排序优先级；等优先级但不同发布内容必须明确选版本，不能凭发布时间决定更新。

### 签名与首次信任

使用独立 Minisign 插件目录签名，不复用 Desktop 私钥。签名覆盖目录的原始字节；先验证签名，再解析或信任其中的元数据。`.pub` 是首次添加的候选公钥，不是自动信任根。

- 官方公钥内嵌宿主；第三方首次添加显示仓库、完整指纹和风险，由管理员确认固定。推荐通过独立渠道核对指纹。
- 第三方公钥与目录来自同一路径时属于 TOFU，不能宣称抵抗首次连接被篡改。需要更强保证时，管理员必须提供独立核对的指纹。
- 公钥变化阻止新安装与更新，包括已经生成但尚未提交的预览；恢复旧公钥的网络响应不能清除阻止状态。重新信任显示旧新指纹，递增信任修订、使旧预览失效并清除旧目录缓存，然后重新检查。已安装插件不会因此被自动卸载。
- 官方密钥轮换通过经验证的宿主更新引入，不通过未认证网络结果替换。
- 现有 SHA256SUMS 只证明与同处发布的摘要一致，不替代签名。Core 使用 `minisign-verify` 验证目录；生产密钥运维由发布维护者负责。

签名不保证目录是最新的。保留本机已见版本/摘要，拒绝自动降级与已见版本内容替换；对首次安装的旧但有效签名目录仍无法完全检测冻结攻击，界面不得把签名有效等同于「最新」。

## 5. updater 与国内外分流

### 路由方案

客户端依赖以下插件专用路由，现有 Desktop 更新路径及语义不变；这些路由在 updater 外部仓库实现：

| 外部路由契约 | 职责 |
| --- | --- |
| `GET /api/v1/plugin-repositories/{owner}/{repo}` | 返回仓库 ID、规范名及是否纳入镜像 |
| `GET /api/v1/plugin-repositories/{owner}/{repo}/releases?per_page=100&page=N` | 分页 Release 元数据，保留插件目录及资产信息 |
| `GET/HEAD /api/v1/plugin-repositories/{owner}/{repo}/releases/{tag}/assets/{filename}` | 提供目录、公钥、签名及已授权压缩资产 |

不能假定已部署的 updater 已提供这些接口。插件路由不得沿用 Desktop 对 `Stravia/<SemVer>` 的应用版本过滤，否则独立插件的新版本会被漏掉。tag 和文件名按路径段编码；服务端重新校验，不能拼接为任意上游 URL。

updater 按数字仓库 ID 固定镜像白名单。官方源与 Qoder（`Chikage0o0/stravia-vendor-qoder`）已纳入；WorkBuddy 在运营者确认后纳入。白名单决定带宽与缓存服务范围，不代表代码审查或安全背书。

- 白名单内：目录通过 updater 获取；每次实际资产 GET/HEAD 根据请求 IP 决定国内缓存交付或境外跳转 GitHub，不能依据早先目录查询地域固定地址。
- 白名单外：返回明确 `not_mirrored` 能力结果，宿主直接读取 GitHub Release 和资产。任意公开第三方仓库仍可添加，但不承诺国内加速。
- updater 网络失败、签名失败或未知错误不得解释成 `not_mirrored`，也不静默换路径；显示错误，允许显式刷新。管理员配置代理或更换分发基址是恢复手段。
- 若要求所有手动添加的仓库无条件国内加速，必须另批准开放镜像的限额、滥用防护与内容治理；不能把客户端「添加源」变成公共服务端自动放行。

### 下载授权与缓存

元数据资产只允许固定名字且有大小上限；组件资产必须来自该仓库/tag 的有效目录，并存在于实际 Release 中、大小一致。updater 不执行 Wasm。目录签名由 Core 必须验证；updater 的解析与资源约束不能替代客户端认证。

缓存身份包含仓库 ID、Release/asset ID 和声明摘要，不能继续只用 `(tag, filename)`；索引刷新后同名资产替换不得错误命中旧内容。所有仓库共享现有磁盘预算与冷下载合并机制，不能每加一个源无界增加缓存配额。

只允许公开 GitHub Release 路径及必要 HTTPS 资产跳转；DNS/每跳目标不得落到回环、内网或链路本地地址。GitHub Token 仅用于 GitHub API，不转发给资产主机或用户输入地址。国内出口分流沿用可信代理 CIDR 和实际连接来源规则，不能相信任意 `X-Forwarded-For`。

## 6. Core 获取与安装链路

### 模块划分

Core 的 marketplace 模块封装来源解析、信任、目录发现、版本选择和有界下载；其外部 interface 只暴露来源预览/确认、来源管理、市场查询/刷新、生成安装预览。HTTP 与 Desktop 共用 Core，不建立 Tauri 专用安装实现。

内部将 Release 获取作为 seam：updater adapter 和 GitHub adapter 提供同形的 Release/asset 数据。只抽取二者确实共用的分页、HTTP 限额与代理配置，不把 Desktop manifest 验证强行泛化成插件验证。

`preview_install(source_id, release_key)` 返回现有 `PluginPreview` 的扩展结果，不返回任意文件给 WebUI。现有 PluginManager 继续拥有 revision 检查、数据兼容、任务排空、落盘和安装事务。市场不直接写插件安装表。

### 安装顺序

1. 检查来源启用状态、固定仓库身份与信任修订号。
2. 获取选定 Release 的目录/签名，校验签名、格式、来源绑定与兼容声明；操作绑定确切版本和摘要，不在点击后悄悄切到另一版本。
3. 通过对应 adapter 下载压缩资产，流式限制输入长度并计算摘要；禁用会改变资产字节的透明 HTTP 内容解码，拒绝非 identity 的 `Content-Encoding`。
4. 完整校验压缩大小和 SHA-256，然后在受限阻塞任务中解压。流式限制输出，不能先无限展开再检查。
5. 校验最终 Component 大小和摘要，执行现有 Wasmtime 准入，并核对 descriptor 身份、版本、canonical 代际及实际 WIT 导入。宿主版本范围是发布者声明，不假装能从 descriptor 反推出。
6. 生成安装预览，增加旧/新来源、仓库、公钥指纹和 Release 链接；保留凭据继承、网络权限变化、降级、数据丢弃及在途操作提示。
7. 管理员确认时重新核对来源信任修订、启用状态、插件 revision 和当前数据状态。来源移除、换钥、配置变化或预览过期均使旧预览失败，不能自动重新预览后继续提交。
8. 使用已验证且与预览绑定的 Component 字节提交，不在确认阶段重新下载可变地址；原子写入安装及来源记录。失败保持旧安装与来源关联不变。

### 资源上限

| 对象 | 上限与处理 |
| --- | --- |
| Release 列表 | 每页 1 MiB、100 条、最多 10 页；到上限仍有更多时显示扫描范围，不声称全历史已扫描 |
| 单 Release 目录 | 256 KiB，最多 100 条插件记录 |
| 公钥 / 签名 | 各 4 KiB |
| 压缩资产 | 声明大小与 64 MiB 两者较小值，额外读取一字节检测超限 |
| 解压结果 | 必须等于声明大小，最大 64 MiB |
| Zstd | 单一标准 frame，无字典，window 最大 8 MiB；拒绝拼接 frame、skippable frame 和尾随字节 |
| 并发安装准备 | 实例内最多两个，排队可取消；限制临时文件与 Wasmtime 准入占用 |

`Decoder::single_frame()` 不足以单独证明没有尾随字节；实现必须检查 frame 类型、完整输入消费和结束状态。`window_log_max(23)` 限制回溯窗口，不是进程总内存上限。Zstd 1.5.7 的 level 19 默认 windowLog 为 23，发布工具仍须检查输出满足契约，不能仅依赖命令行参数名。

本地压缩导入接受 `application/zstd`，原始导入继续接受 `application/wasm`。按 MIME 与实际 magic 一致性判定，不只看后缀。本地导入无受信任目录时依然执行相同资源及组件准入限制，但来源为 `local`，不声称验证发布者身份。

## 7. 版本选择与检查更新

- 从启用来源的已发布非草稿 Release 扫描目录；预发布需来源显式开启，Release 的 prerelease 标记和插件 SemVer 预发布任一成立即视为预发布。
- 按插件 SemVer 选择扫描范围内最高兼容版本，不依赖 GitHub `/latest`、tag 字符串或发布时间排序。
- 同时显示发现的更高但不兼容版本及所需宿主版本，不能把「需升级宿主」说成「已最新」。实际 ABI 准入仍是最终判断。
- 自动检查只生成可用版本状态，不下载全部 Wasm、不执行未选中的组件、不自动提交安装。
- 启动后检查到期来源，之后每小时判断一次；成功缓存 24 小时，失败保留旧展示数据并标明最近成功时间。手动刷新可立即触发，但同一来源的并发刷新合并，遵守 GitHub 限流提示，不做盲目重试。跨实例用数据库分配的检查序号阻止旧结果覆盖新结果，不依赖秒级时间戳排序。
- 单源失败不遮蔽其他来源结果；不存在目录的旧 Release 可跳过，存在目录但签名/格式错误必须显示来源异常，不能伪装成没有更新。
- 正常更新只允许同来源的严格更高版本。同版本不同摘要仅阻止该来源的冲突版本，不阻止其他正常版本；降级与换源均为独立显式操作。缓存视图只读取已见摘要记录，不在每次页面查询时重写它。
- 请求复用现有 `update_uses_proxy` 与出站代理设置；不继承插件连接的凭据、Cookie 或 OAuth Token。独立 GitHub 直连同样遵守该代理设置。

## 8. 管理 API 与界面

### 管理 API

均在既有 `/api/v1` 管理权限下，沿用 Server CSRF 与 Desktop 本地认证，响应 `no-store`。安装准备支持阶段及真实字节进度，取消不会提交安装。

| 方法 / 路径 | 结果 |
| --- | --- |
| `GET /plugin-sources` | 来源及信任、镜像能力、最近检查状态 |
| `POST /plugin-sources/preview` | 输入 GitHub URL；返回短期 `preview_id`、仓库身份、指纹、可发现插件及风险，不保存来源 |
| `POST /plugin-sources` | 确认预览并保存固定来源；预览绑定 URL、仓库 ID、公钥和管理员会话 |
| `PATCH /plugin-sources/{id}` | 启停与预发布偏好，不能顺便换仓库或换钥 |
| `DELETE /plugin-sources/{id}` | 删除第三方来源，已安装插件保留 |
| `POST /plugin-sources/{id}/trust-preview`、`.../trust-confirm` | 显式重新信任候选公钥，不复用普通设置保存 |
| `GET /plugin-marketplace` | 已缓存的聚合视图；首次无缓存显示未检查，不把查询变成无界下载 |
| `POST /plugin-marketplace/refresh` | 检查指定或全部来源，返回每源结果 |
| `POST /plugin-marketplace/preview-install` | 按来源与精确发布记录获取并生成安装预览 |
| `POST /vendor-plugins/confirm` | 复用现有安装确认，增加来源修订检查 |

来源预览与安装预览使用 15 分钟有效期、有限数量与会话隔离。对于首次确认之后网络结果发生的仓库/公钥变化，返回预览失效，而不是静默更新被确认的内容。稳定错误类别区分：未适配市场、来源身份变化、签名失败、版本冲突、不兼容、限流、网络失败、下载损坏、资源超限和预览过期；面向用户给恢复动作，不暴露内部堆栈。

### 页面结构

「供应商插件」使用统一工作台，每个 Vendor×来源一行，不再把已安装与市场拆成平行列表：

```text
供应商插件                                   检查更新  插件源  ⋯
[全部] [已安装] [有更新]
搜索名称或 Vendor ID                          来源筛选
插件            来源             已安装   可用          状态        操作
OpenAI Codex    官方             —        0.6.4 1 MiB   ○ 可用      安装  详情
WorkBuddy       Chikage0o0/…     0.4.0    0.5.0 1 MiB   ▬ 有更新    更新  详情
```

- 页签是同一列表的状态筛选；保留卸载和恢复内置操作，内置 `base` 不出现市场升级动作。本地导入收进页头「更多操作」菜单。
- 状态使用带形状的状态标签（最新/已安装为圆点，有更新、来源异常、需升级宿主为横条，版本冲突为菱形），不兼容原因紧随状态。只有安装与更新是实心主按钮；重新安装、替换来源等低频动作为 outline，「详情」为 ghost，行内操作不换行。
- 列表使用有行线的表格，不做商品卡片墙。详情 Sheet 以标签/值列表展示说明、自报作者、仓库、指纹、已安装版本的来源、Release、摘要与安装时间，以及可选择安装的版本列表；来源停用不显示为「已是最新」。打开 Sheet 与安装确认时，初始焦点分别落在关闭与取消，避免技术值提示气泡自动弹出。
- 「插件源」打开独立 Sheet，显示仓库、官方标记、启用和预发布开关、国内加速能力、最近成功检查、错误及重试。「官方」仅来自内置身份，不来自名称、SVG 或作者字段。
- 添加来源分两步：输入仓库 → 预览仓库/公钥/插件并确认信任。添加来源不安装插件；安装另行展示凭据与网络影响。
- 安装按钮经过下载校验后进入现有确认 Sheet；取消不变更安装。进度只在字节总量已知时显示百分比，校验/准入使用阶段状态。
- 未适配仓库显示「该仓库尚未发布插件目录」，提供 Release 链接和发布规范；不根据 `.wasm` 文件名偷偷降级为无验证市场安装。
- 允许离线查看已安装与旧缓存，但过期/失败常驻显示；无源、无插件、无搜索结果与刷新失败分别呈现。

视觉遵循 [DESIGN.md](../../DESIGN.md)：钢蓝语义 token、IBM Plex Sans/Condensed/Mono 与中文 Noto Sans SC、既有 PageHeader/Tabs/DataTable/Sheet；不引入新配色、字体或装饰卡片。技术值才用等宽。图标作为图像使用现有 Provider mark，不注入 SVG 到 DOM。320px 下转为移动列表，保留来源和主操作；中英文、键盘焦点、明暗主题及状态文字同时验证。

## 9. 发布者接入与持久化

### 官方与 WorkBuddy 共用发布工具

在 Platform devtools 提供单一目录生成/校验命令，输入最终 Wasm、显式宿主版本范围、仓库 ID/tag 与发布信息；读取真实 descriptor/WIT、执行准入、压缩、计算双摘要，输出目录。工具不从文件名猜身份，也不从 README 抽取兼容范围。

签名作为独立发布阶段，由受保护 CI 凭据完成；第三方使用同一 `stravia-tools plugin-catalog` 命令，不必复制官方打包脚本。工具在签名前完成解压回环与 descriptor 一致性检查，目录生成后不得重写字节。命令、输入格式与签名步骤见[发布说明](../releasing.md#vendor-plugin-目录与签名)。

官方工作流：构建九个专属组件 → 生成 `.wasm.zst` 和目录 → 签名 → 将全部资产上传 draft → 核对实际资产清单 → 公开 Release。`base` 不进入目录。可继续保留 Release 级 SHA256SUMS，但不当作市场认证。

WorkBuddy 的外部维护者需采用同一流程，目录身份固定 `workbuddy`，host 范围由维护者声明。历史 Release 不重写、不补造签名，以新 Release 接入。现有代码固定 SDK Git revision 的方式不变；图标分发权利需维护者确认。其旧 `.wasm` 仍可手工导入，发布新的 `.wasm.zst` 后可经宿主本地压缩导入承接离线场景。

### 持久化

增加来源表、目录缓存/检查状态以及安装来源关联；来源 ID 与来源快照分开，移除来源不能级联删除插件。缓存不是信任根，签名原始字节和固定公钥必须能再次校验。已见版本/摘要用于检测替换，设置明确容量及历史保留策略，不能把无限 Release 历史全部写入通用 settings。

既有 `builtin/local` 数据不凭空回填市场来源。SQLite/PostgreSQL 均通过增量迁移 `0052_vendor_plugin_marketplace.sql` 增加结构，保持 revision CAS 与已有安装事务。多实例共享数据库时，确认事务同时校验来源信任、启用状态及已见版本冲突；来源锁将检查与换钥、冲突记录串行化。参考结构见 [SQLite](../database/sqlite.sql) 与 [PostgreSQL](../database/postgres.sql)。

## 10. 外部上线前置与验收

1. 发布维护者配置独立插件目录公钥、签名私钥和签名密码；私钥不进入宿主、镜像或仓库。缺失配置不得发布伪造的官方源。
2. updater 维护者部署已实现的插件路由、白名单与缓存隔离（v2 schema 与样例已发布在 updater 仓库），保留 Desktop 路由契约。
3. 官方及 WorkBuddy 维护者用同一工具生成产物并分别签名，发布新的兼容 Release；部署和公开发布需单独授权。
4. 上述真实服务与发布材料就绪后，再验收生产分流和首次官方安装。本地隔离测试不宣称已经验证这些外部生产条件。

验收必须覆盖：

- 默认官方源能列出专属插件，添加 WorkBuddy 后无需手工下载即可预览安装；两者走同一安装协调器。
- 官方及纳入白名单的 WorkBuddy 在模拟 CN/非 CN 出口分别得到缓存交付/正确 GitHub 跳转；未知地域与 HEAD 保持既有分流规则，缓存不跨仓库串包。
- 未纳入镜像的公开仓库可以直连添加，界面明确无国内加速；镜像故障不伪装成未收录。
- 自动发现同来源新版本，管理员确认后升级；其他来源同名插件、新增权限、数据重置、降级与换钥均不能静默生效。
- 压缩/解压摘要错误、截断流、超大窗口、字典、多个 frame、尾随字节、解压炸弹、未知 ABI 和 descriptor 不匹配全部在安装提交前失败。
- 篡改目录/签名、仓库名复用、来源删除或换钥后的旧预览、版本内容替换被拒绝；已安装可用版本与凭据保持原状。
- GitHub 分页、限流、部分来源失败、离线缓存与较新但不兼容版本的 UI 状态准确；到扫描上限不声称完整。
- SQLite/PostgreSQL 验证来源迁移、事务失败回滚与 revision 竞争；Server/Desktop 各自通过真实 HTTP 管理路径操作；WebUI 用真实浏览器验证安装确认、窄屏、语言和键盘路径。

## 11. 调研来源与外部边界

设计基于当前工作树和以下外部本地仓库的调查；updater 的插件路由已在其仓库实现但未部署，其他外部仓库不代表已完成市场实现：

- `/home/chikage/projects/updater/docs/protocol/plugin-marketplace-v2.md`、`schemas/plugin-catalog-v2.schema.json`：v2 目录契约。
- `/home/chikage/projects/updater/src/plugins.rs`、`src/service.rs`、`src/upstream.rs`、`src/cache.rs`、`README.md`：多仓库插件路由、Desktop 分流、下载授权与缓存。
- `/home/chikage/projects/stravia-vendor-workbuddy/.github/workflows/release.yml`、`Taskfile.yml`、`Cargo.toml`、`src/profile.rs`、`README.md`：现有第三方构建、元数据和手工安装流程。
- [GitHub Release REST API](https://docs.github.com/en/rest/releases/releases)、[API 限流](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)：发现与限流契约。
- [Zstd Decoder 0.14.3](https://docs.rs/zstd/0.14.3/zstd/stream/read/struct.Decoder.html)、[Zstd 1.5.7 参数表](https://github.com/facebook/zstd/blob/v1.5.7/lib/compress/clevels.h)：单 frame 与 window 限制能力。

独立目录签名/TOFU、公开 API、持久化与 Core 签名依赖已按批准规格在本仓库实施；updater 的插件路由与白名单已在其仓库实现。其余外部仓库改动、生产密钥生成与配置、发布和部署不在本次实现范围；测试密钥仅用于隔离夹具，不作为任何生产信任根。
