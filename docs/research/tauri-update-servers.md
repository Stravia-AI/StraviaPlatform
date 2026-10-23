# Tauri 更新服务器品类与选型

调研日期：2026-10-23。范围：Tauri 2 的原生 updater、更新元数据服务与安装包分发。依据为官方文档、服务商文档和候选项目源码；本次没有部署候选服务，也没有执行真实制品下载、验签或安装，不能据此声称生产兼容性已通过验证。

## 结论

**协议只有两类，部署方案可分四类。** Tauri 官方支持静态 JSON 和动态更新接口；GitHub Releases、对象存储、Serverless、自建发布平台、托管 SaaS 是这些协议的不同承载方式，不是五套 updater 协议。[S1]

对 Stravia，当前优先保留 GitHub Release + 签名静态清单。只有明确需要独立分发网络、服务端发布策略或私有发行时，才值得引入新的服务。若想减少分发运维，优先评估 CrabNebula Cloud；若要完全自托管发布管理，评估 FaynoSync；若只是增加少量动态决策，薄 Worker/API 比完整发布平台更小。这些是选型建议，不是性能或可用性实测结论。

## 1. 四类部署方案

| 品类 | 代表 | 提供什么 | 主要边界 |
|---|---|---|---|
| 静态清单与文件托管 | GitHub Releases + `tauri-action`；S3、R2 或其他 HTTPS 静态托管 | 托管 JSON、更新包与签名；客户端选平台、比较版本 | 运维最少；多份清单可以分频道，但清单本身不负责按用户分桶或版本授权；私有访问由托管层另行解决。[S1][S2] |
| 薄动态接口／Release 代理 | Cloudflare Worker、小型自建 HTTP API；`mackenly/tauri-update-cloudflare` | 根据当前版本、OS、架构转换上游 Release 信息并返回动态响应 | 制品可以继续留在 GitHub；上游可用性、API 配额、资产命名与缓存仍需处理。Serverless 不等于不需要维护。[S4][S5] |
| 完整自托管发布平台 | FaynoSync | 发布记录、频道、制品存储、管理后台与框架专用更新接口 | 自己管理服务、数据库、缓存、存储和备份；具体策略是否适用于原生 Tauri 客户端必须分别核验。[S6][S7] |
| 托管分发平台／SaaS | CrabNebula Cloud、Mateality Releases；带许可证管理的 Keygen | 托管制品、更新接口与发布管理；各家的频道、分析、授权能力不同 | 服务政策、历史制品保留、费用与供应商依赖；不能把所有平台都视为支持灰度或自动降级。[S8]–[S14] |

“自己写一个更新服务器”属于第二或第三类实现选择，不需要另立一种协议。许可证控制也是能力维度，而不是互斥的托管类别：Keygen 同时提供 Cloud 与 self-host 选项。[S13][S14]

## 2. 两种原生协议

### 静态 JSON

返回包含 `version` 与 `platforms` 的 JSON。每个平台条目包含 `url` 和 `signature`，键通常为 `windows-x86_64`、`linux-aarch64`、`darwin-aarch64` 等 `OS-ARCH`。`notes`、`pub_date` 可选。客户端比较版本；Tauri 在比较前会校验整份清单，因此已列出的平台条目必须完整。[S1]

官方给出的 GitHub endpoint 是 `https://github.com/user/repo/releases/latest/download/latest.json`。`tauri-action` 提供 `uploadUpdaterJson`，自动生成并上传 updater JSON；这仍然是静态方案，不是 GitHub 提供了动态 Tauri 更新 API。[S1][S2]

### 动态接口

Endpoint 可以使用 `{{current_version}}`、`{{target}}`、`{{arch}}`。有更新时返回 `200` 和单平台的 `version`、`url`、`signature`；无更新时返回 `204 No Content`。需要认证或额外决策信息时，客户端可以增加请求头。[S1]

频道可以体现在 URL 或查询参数中。百分比灰度则需要实际的决策机制；Tauri 标准响应没有一个自动生效的通用 `rollout` 或 `critical` 字段。仅平台宣称支持灰度、或者返回额外字段，不足以证明原生 Tauri 客户端会执行该策略。[S1]

### 不随服务器品类变化的约束

- **签名校验不可关闭。** `signature` 是 `.sig` 文件内容，不是签名文件 URL。服务商托管安装包，不意味着需要托管签名私钥。[S1]
- 生产环境默认要求 HTTPS；不应以危险传输开关替代正确部署。[S1]
- **下架坏版本不等于已升级客户端自动降级。** 默认比较器只接受更高版本；真正降级必须事先在客户端设计 `version_comparator`，并核对应用数据兼容性。[S1]
- Tauri 2 默认使用 Windows `.exe`/`.msi`、Linux `.AppImage` 与 macOS `.app.tar.gz` 更新产物。旧服务器如果只按 v1 的压缩包后缀匹配，不能直接视为兼容。[S1]
- updater 插件面向 Windows、Linux、macOS，不支持 Android/iOS；Web 资源 OTA 与整个桌面应用的二进制更新也不是同一个问题。[S1]

## 3. 代表方案核验

| 方案 | 已核实的能力 | 成本／许可 | 限制与可信度 |
|---|---|---|---|
| GitHub Releases + `tauri-action` | 官方构建发布工具；可上传 updater JSON 与签名制品 | 无新增更新服务；CI、存储和网络仍受宿主规则约束 | 官方路径；适合公开分发。静态多频道需自己组织清单与入口。[S1][S2][S3] |
| `mackenly/tauri-update-cloudflare` | Worker；按 target、arch、当前版本返回更新；上游调用 GitHub `releases/latest` | MIT；Worker 与上游成本另计 | README 自称维护中的 fork，但使用说明仍链接 Tauri v1。源码能看到动态协议形态；不能据此认定 Stravia 四平台与私有下载已通过验证。[S4][S5] |
| FaynoSync | 仓库明确支持 Tauri、频道、S3 兼容存储、管理后台；源码包含 Tauri 签名参数校验与更新包选择 | Apache-2.0；自行承担基础设施 | 标准栈含 Go API、MongoDB、Redis、S3 兼容存储。当前通用 `checkVersion` 文档仍未列出 Tauri 响应示例，存在文档与源码覆盖差异；接入前应锁定版本并做原生 updater smoke。[S6][S7] |
| CrabNebula Cloud | Tauri 官方合作伙伴；CDN、updater 集成、多频道、下载指标、CLI/GitHub Action | 自 2026-06-19 起免费；异常大规模使用另议 | 90 天无下载的 Release 会清理，但每个频道最新 Release 受保护；存在未给固定数字的 fair-use 政策。[S8][S9][S10] |
| Mateality Releases | 官方明确给出 Tauri v2 配置、按 OS/架构选择、动态 `200`/`204`、CI 上传与下载页 | Hobby €0/月；Indie €10/月；Team €30/月；Business €60/月，未含税 | Hobby/Indie 为滚动保留；Team 起保留完整历史但受存储额度约束。官方说明私钥留在 CI。未做实际发布验证。[S11][S12] |
| Keygen | 官方 Tauri engine；按平台、架构、版本、频道和版本约束选择；支持 license/token 授权 | Tauri engine 位于 Cloud Std/Ent；另有自托管选项 | 更适合“买了许可证才能获得相应更新”。官方示例仍为 Tauri v1 配置；迁移到 v2 的可行性属协议层判断 `[INFERENCE]`，不是本次实测。当前静态定价页未显示基础套餐金额，因此不引用具体起价。[S13][S14] |

### 两个容易误判的地方

1. **“Tauri 更新服务器”这个仓库名不足以证明可直接用。** 例如 `tauri-chipmunk` 当前源码只按 OS 与扩展名选第一个匹配资产，没有架构维度；同一 Release 同时含 x86_64 与 ARM64 时，不能保证发对包。不应直接用于 Stravia 的四平台发行。[S15]
2. **“支持回滚／灰度”不等于标准客户端完整支持。** 要核对决策发生在服务端还是专用 SDK，是否需要额外设备标识，以及已安装客户端是否允许降级。不能只看平台功能列表。[S1]

## 4. Stravia 当前在哪里

本仓库属于“**GitHub Release 发现 + 每个版本的静态签名清单**”，不是纯粹将 `latest.json` 写进 Tauri 配置：[R1]–[R4]

- Core 的 `updates.rs` 固定查询 `Stravia-AI/StraviaPlatform/releases`，按版本与 prerelease 规则选择 Release，再读取名为 `stravia-updater.json` 的 asset。
- 清单生成器为四个平台写入精确版本的 GitHub asset URL、内联签名和 `release_notes_url`。
- Core 校验清单版本与选中 Release 一致、Release notes URL 一致、四个平台完整且没有额外平台，并要求 HTTPS 与非空签名。
- Desktop 从 `STRAVIA_UPDATER_PUBLIC_KEY` 编译期注入公钥，在运行时设置选中清单的 endpoint；不是依赖 `tauri.conf.json` 中空的 `pubkey`。
- Desktop 再确认 updater 返回的版本与用户选中版本一致，然后执行带验签的下载与安装流程。

因此，**换 SaaS 不是仅修改 `plugins.updater.endpoints`。** 当前发现层与清单校验契约也需要一起迁移；通用动态响应只有单个平台的 `url`/`signature`，不能不改 Core 就替换现有四平台清单。[R1][R2]

同样，**只把安装包放到 CDN，并不能移除检查更新时的 GitHub API 依赖**。GitHub 未认证 REST API 按来源 IP 限制为每小时 60 次；当前 Core 的发现请求未配置认证。若问题是共享出口下的 API 配额或 GitHub 连通性，应先区分发现请求与制品下载，再决定迁移哪一层。[R1][S16]

签名信任根继续遵循现有 ADR：保留专用 updater 密钥与离线备份，不为了换托管平台引入远端任意换公钥或关闭验签的路径。[R5]

## 5. 推荐路径

1. **只需要公开稳定更新：保持现状。** 已有签名、精确版本清单和四平台发布约束，没有必要再增加数据库和更新后台。
2. **主要目标是独立下载网络、统一频道与少运维：先评估 CrabNebula Cloud。** 当前免费是优势，但必须接受并核对历史制品清理与 fair-use；不能把它当永久制品档案库。[S8]–[S10]
3. **只需要少量服务端策略或上游代理：选薄 Worker/API。** 可以复用现有签名制品；必须明确 OS/架构映射、稳定版与预发布选择、鉴权和上游依赖。现成 Worker 仅作候选，不默认按其 README 直接生产上线。[S4][S5]
4. **确实需要完全掌控发布后台、私有制品和多应用管理：评估 FaynoSync。** 先接受 MongoDB、Redis、S3 等运维成本，再验证目标版本的 Tauri 原生适配。[S6][S7]
5. **已有商业许可证与升级权益需求：再看 Keygen。** Stravia 当前问题不是授权售卖，不应仅为自动更新顺带引入许可证体系。[S13][S14]

后续若进入接入实施，验收面应覆盖四个平台的检查、无更新、平台与架构选择、损坏签名、下载、安装与重启后版本；另测稳定版/预发布选择，以及所选平台声称提供的认证或灰度策略。本次仅研究，不改产品代码、发布流程、密钥或线上服务。

## 6. 国内外分流：美国 CN2 节点只做镜像与缓存

用户明确的边界是：**面向国内用户的节点也在美国，通过 CN2 链路服务国内访问；节点只承担镜像和缓存。** 这不是部署中国大陆 CDN，也不需要为此引入 FaynoSync、数据库或独立的更新业务后台。以下是推荐设计，尚未部署或测速。

### 最小结构

保留 GitHub Release 作为唯一发布来源，使用自有 HTTPS 入口：

- 小体积的 Release 元数据和版本化清单可以共用美国 CN2 节点；节点缓存固定仓库的公开元数据，不负责选版本或改变发布状态。
- 安装包使用统一版本化 URL。入口按请求来源地区选路：国内请求返回本机镜像文件；海外请求临时重定向到对应的 GitHub Release asset。地域判断只负责交付，不承担 updater 业务规则。
- 发布时把同一组已签名的四平台制品、签名和清单同步到镜像目录；文件完整就绪后再对外宣告镜像版本可用。镜像节点不保存签名私钥，不重新构建或签署安装包。

这个结构让海外安装包的大流量直达 GitHub，但**海外的小体积检查请求和下载入口仍经过 CN2 节点**；它仍是共享入口的单点。如果要求海外完全绕过该节点，需要另配海外入口与地域 DNS，而不是假装一个 GitHub 域名 CNAME 就能承载自有 HTTPS 域名和路径。地域 DNS 还受到递归解析器位置与 EDNS Client Subnet 支持的影响，不保证准确识别用户所在地。[S19]

默认不增加客户端测速、竞速下载或新的灰度机制；代理/VPN 会改变服务端看到的出口位置，地域识别不能使用 UI 语言代替。

### 镜像／缓存的正确边界

| 对象 | 建议处理 |
|---|---|
| Release 列表 | 缓存固定公开仓库的响应；分页参数进入 cache key；保持原有 stable/prerelease 选择规则，不把上游错误缓存成成功列表 |
| 版本化 updater 清单 | 保留版本、平台与签名；下载地址指向自有入口。可以在发布时生成，不需要请求时用字符串替换 JSON |
| 安装包与 `.sig` | 优先发布时完整同步后静态分发；按版本路径管理，不覆盖同版本的已发布字节 |
| 懒加载文件缓存 | 如果采用，必须取得最终文件正文；GitHub 下载接口可能返回 `200` 或 `302`，不能仅缓存并透传上游重定向，否则国内下载又回到 GitHub。[S18] |

Nginx 可承担静态文件交付与元数据反向代理缓存；`proxy_cache_lock` 可合并同一冷缓存项的回源请求，`proxy_cache_revalidate` 支持条件再验证。具体缓存期限、上游响应头与错误状态处理要在部署配置中明确，不能仅写一个 `proxy_pass` 就认定完整镜像已成立。[S17]

镜像只允许固定仓库、固定资源路径，不提供任意目标 URL 的公开代理；海外重定向目标也由已知版本与资产映射产生。所有入口保持 HTTPS；字节不变即可沿用原始 Tauri 签名。[S1]

### 本仓库需要调整的位置

这仍然不是零代码切换：[R1]–[R3]

1. Core 的 Release 发现请求改走固定元数据镜像入口；选中清单的地址也必须转到镜像，不能继续使用 GitHub API 响应中的原始 `browser_download_url`。
2. 清单生成器把安装包 URL 改为自有分发入口，保留原始签名、精确版本和四平台完整性；`release_notes_url` 可以继续指向原始 GitHub Release。
3. 发布流程同步签名制品与清单到镜像，不能让 CN2 节点重新决定哪个版本是最新版本。

仅代理 Release API 不够：其清单 URL 仍可能指向 GitHub。仅镜像清单也不够：其中的安装包 URL 仍可能绕过镜像。选路应覆盖“发现 → 清单 → 制品”，同时保留现有验签与版本核对。

真正实施时再验证国内出口返回镜像正文、海外出口重定向到正确资产、四平台 URL 映射、冷/热缓存和损坏签名拒绝；本次没有对应服务器、DNS 配置或实际链路证据，不声称已实现分流或获得加速。

## 来源

### 第一方外部来源

- [S1] [Tauri 2 Updater：协议、签名、产物、运行时配置与降级比较器](https://v2.tauri.app/plugin/updater/)
- [S2] [Tauri GitHub Action：`uploadUpdaterJson` 与发布行为](https://github.com/tauri-apps/tauri-action)
- [S3] [GitHub Releases：制品与带宽规则](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases)
- [S4] [mackenly/tauri-update-cloudflare：定位、部署与许可入口](https://github.com/mackenly/tauri-update-cloudflare)；[MIT 许可证](https://github.com/mackenly/tauri-update-cloudflare/blob/main/LICENSE.md)
- [S5] [Worker 动态响应与平台选择](https://github.com/mackenly/tauri-update-cloudflare/blob/main/src/handler.ts)；[GitHub latest Release 请求](https://github.com/mackenly/tauri-update-cloudflare/blob/main/src/services/github.ts)
- [S6] [FaynoSync：支持范围、技术栈、频道与许可](https://github.com/ku9nov/faynoSync)
- [S7] [FaynoSync Tauri 适配源码](https://github.com/ku9nov/faynoSync/blob/main/server/utils/updaters/tauri.go)；[当前 checkVersion 文档](https://faynosync.com/docs/api/info/latest)
- [S8] [Tauri 官方 CrabNebula Cloud 集成说明](https://v2.tauri.app/distribute/pipelines/crabnebula-cloud/)
- [S9] [CrabNebula 当前计费与使用政策](https://docs.crabnebula.dev/cloud/org-management/billing/)
- [S10] [CrabNebula 2026-06-11 公告：免费、90 天清理与频道最新版本保护](https://crabnebula.dev/blog/why-cloud-is-free/)
- [S11] [Mateality Releases Tauri v2 接入与响应协议](https://releases.mateality.com/docs/tauri-v2)
- [S12] [Mateality Releases 定价、保留规则与签名密钥归属](https://releases.mateality.com/pricing)
- [S13] [Keygen Tauri engine：频道、版本约束、签名与认证](https://keygen.sh/docs/api/engines/#engines-tauri)
- [S14] [Keygen 定价与功能分层、自托管入口](https://keygen.sh/pricing/)
- [S15] [tauri-chipmunk：按 OS 与后缀选资产的实现](https://github.com/kognise/tauri-chipmunk/blob/main/src/lib.ts)
- [S16] [GitHub REST API rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)
- [S17] [Nginx HTTP proxy module：缓存键、缓存锁与再验证](https://nginx.org/en/docs/http/ngx_http_proxy_module.html)
- [S18] [GitHub Release assets：下载正文与重定向语义](https://docs.github.com/en/rest/releases/assets)
- [S19] [Route 53：递归解析器与 EDNS0 的地域判断边界](https://docs.aws.amazon.com/Route53/latest/DeveloperGuide/routing-policy-edns0.html)

### 仓库依据

- [R1] [`updates.rs`：GitHub 来源、Release 选择和清单校验](../../backend/crates/stravia-core/src/admin/updates.rs)
- [R2] [`product_update.rs`：运行时公钥、endpoint、精确版本核对和下载](../../backend/apps/stravia-desktop/src/product_update.rs)
- [R3] [`generate_updater_manifest.py`：四平台签名清单生成](../../.github/scripts/generate_updater_manifest.py)
- [R4] [现有 Desktop 发布与 smoke 约束](../releasing.md)
- [R5] [ADR-0039：固定 Desktop 更新签名密钥](../adr/0039-pin-tauri-updater-signing-key.md)
