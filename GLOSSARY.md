# Stravia 领域术语词典

本词典定义 Stravia 的核心业务与领域概念，统一中文表述与代码/协议中的规范英文名。

- **本文件只回答「是什么、不是什么、彼此是什么关系」**，不包含实现细节、代码模块或配置步骤。
- 详细模块契约见 [`docs/design/`](docs/design/README.md)；架构决策与取舍见 [`docs/adr/`](docs/adr/README.md)。

## 核心概念全景图

先看懂概念之间如何串联，再查阅具体词条：

```text
接入客户端（Connect Client） / 控制台对话（Console Chat）
        │ 持 Stravia API Key → 建立认证主体（Principal）
        ▼
  路由（Route，以 Route ID 对外暴露，限定单一路由用途）
        │ 按 亲和性 → 目标优先级 → 调度策略 选路
        ▼
  路由目标（Target = 上游连接 Provider + 供应商模型 Provider Model）
        │ 经由 供应商插件（Vendor Plugin / Wasm）发往上游服务
        ▼
  推理运行（Inference Run）
    ├── 模型轮次（Model Turn）── 可逆脱敏 / Hook / 协议转换 / 客户端投影
    ├── 平台工具（Platform Tool）── StraviaRead / 联网搜索 / 多模态理解 / 媒体生成 / 历史回看
    ├── 生成链（Generation Chain）── 已完整交付的不可变历史图（用于续接与上下文恢复）
    └── 交互观测（Interaction Observation）── 独立于执行的诊断视图、调试抓包、用量与计费
```

### 常见易混概念速查

| 容易混淆的几组概念 | 一句话区分 |
| --- | --- |
| **模型厂商（Lab） / 供应商（Vendor） / 上游连接（Provider） / 路由（Route）** | **Lab** 是模型作者（如 Anthropic）；**Vendor** 是对接协议与认证的代码插件；**Provider** 是管理员保存的一条带凭据的账号连接；**Route** 是暴露给客户端调用的逻辑模型名。 |
| **失败的请求 / 错误的请求 / 上游错误** | **错误的请求**在发往上游前被平台校验拒绝（不计入可用率）；**失败的请求**是客户端最终因平台或上游故障失败；**上游错误**是单次发往上游的尝试失败（可能已被重试或切换吸收）。 |
| **估算成本 / 计费 / 供应商额度** | **估算成本**按上游价格估算本实例应付给上游的钱；**计费**按 Route 定价向 API Key 内部记账并驱动消费上限；**供应商额度**是上游账户实时上报的配额或余额。 |
| **可逆脱敏 / 调试抓包过滤** | **可逆脱敏**在发给模型前把凭据换成占位符、回来后再还原；**调试抓包过滤**只遮盖 HTTP `Authorization` 请求头，不保证清理其他凭据。 |
| **资产引用 / 下载授权** | **资产引用**（`stravia://artifacts/...`）是不透明身份标识，本身不授予访问权；**下载授权**是带有效期的签名链接，持有即可在有效期内下载。 |

## 目录

1. [产品、运行形态与管理入口](#1-产品运行形态与管理入口)
2. [主体、访问控制与流量准入](#2-主体访问控制与流量准入)
3. [供应商、插件与模型清单](#3-供应商插件与模型清单)
4. [路由、调度与容错](#4-路由调度与容错)
5. [协议、推理与上下文历史](#5-协议推理与上下文历史)
6. [内置 Agent、平台工具与文件资产](#6-内置-agent平台工具与文件资产)
7. [凭据保护（可逆脱敏）](#7-凭据保护可逆脱敏)
8. [交互观测、调试、用量与计费](#8-交互观测调试用量与计费)

---

## 1. 产品、运行形态与管理入口

### Stravia
本地运行、可自托管的 Agent 基础设施（Agent infra），统一提供模型接入、平台工具与内置 Agent 执行、访问控制、历史与可观测性。协议网关只是其模型接入层；Stravia 不是用户自定义 Agent 或可视化工作流搭建器。它是本产品的唯一品牌名，不保留旧名或别名。

### Stravia 桌面端（Stravia Desktop）
运行在本机的 Tauri 桌面管理壳，与独立无头服务端（Server）共用同一 Rust 核心。只有桌面端可以直接改写本机接入客户端的配置文件。
- **避免混用**：单独称作「客户端」（未区分桌面端与第三方编程工具时）。

### 接入客户端（Connect Client）
用户本机上可配置为以 Stravia 为模型服务的第三方编程工具（如 Claude Code、Codex CLI、Gemini CLI、OpenCode）。它既不是认证主体（Principal），也不是内置 Agent 定义。
- **避免混用**：Agent（指第三方工具时）、把桌面端和第三方工具混称为「客户端」。

### 客户端全局配置与一键接入（Connect Client Global Config / Connect Client Apply）
- **客户端全局配置**：接入客户端的用户级配置文件（按各工具官方环境变量与默认目录解析），不含仓库级项目配置。
- **一键接入（Connect Client Apply）**：Stravia 桌面端向客户端全局配置增量写入（upsert）Stravia provider 配置的操作；默认不覆盖用户的当前/默认模型（Claude Code 环境变量映射除外）。它不是服务端 HTTP 管理接口。
- **避免混用**：一键覆盖配置、项目级 `.claude/settings.json` 或 `.codex/config.toml`。

### 控制台对话（Console Chat）
管理员在管理界面中，选定一个 API Key（即对应 Principal）与模型进行的多轮对话。请求与普通接入客户端一样走网关、计入用量与观测；对话所属 Key 创建后不可更换（Key 失效后变为只读），模型可逐条切换。历史保存在浏览器或桌面端本地，不是平台 Session 或生成链视图。
- **避免混用**：Playground、Agent 对话、Session、接入客户端。

### 管理用户与实例管理员（Admin User / Instance Admin）
- **管理用户（Admin User）**：访问管理控制台与管理 API 的身份，与调用模型和 MCP 的 Principal 严格分离；管理用户身份本身不授予模型调用或 MCP 权限，API Key 也不归属于管理用户。
- **实例管理员**：开源社区版默认单管理员模式下唯一的管理用户，拥有实例全部管理权限。
- **避免混用**：Principal、接入客户端、API Key 所有者、上游账号。

### 管理入口与受信代理（Admin Origin / Trusted Proxy）
- **管理入口**：浏览器访问管理面的外部源（协议 + 主机 + 有效端口，不含路径），不同于服务内部监听地址或反代回源地址。
- **受信代理**：部署者明确授权、可向 Stravia 传递浏览器侧原始请求头（如 `X-Forwarded-*`）的反向代理；受信代理本身不授予管理员身份。
- **避免混用**：服务监听地址、已认证客户端。

### 设置令牌与初始化会话（Setup Token / Bootstrap Session）
- **设置令牌**：领取首次初始化资格的一次性凭据，仅用于选择存储后端并创建首个实例管理员，不能用于日常管理或覆盖已有管理员。
- **初始化会话**：凭设置令牌建立的受限上下文，仅用于完成首次初始化流程。
- **避免混用**：Admin Token、API Key、日常管理会话。

### 可用更新与跳过版本（Available Update / Skipped Version）
- **可用更新**：版本高于当前安装且符合发布通道的正式 Release（稳定版只接收稳定版；预发布版接收更新的预发布版或更高稳定版）。
- **跳过版本**：实例对某个精确版本关闭自动更新提醒的共享偏好，不隐藏手动检查结果，出现更高可用更新时自动失效。
- **避免混用**：最新版（未判通道时）、关闭更新、永久忽略更新。

---

## 2. 主体、访问控制与流量准入

### 主体（Principal）
由有效 Stravia API Key 建立的认证客户端身份，是生成链、轮次链、文件资产、脱敏映射与请求频率限额的隔离边界。Stravia 不存在匿名主体，也不用网络连接或 Session 代替主体身份。
- **避免混用**：匿名主体（Anonymous Principal）、Client Session。

### 根请求与 API Key 入口限流（RootRequest / API Key Root RPM）
- **根请求（RootRequest）**：一次已准入的客户端请求所共享的执行上下文，贯穿其内部的隐藏模型轮次、平台工具调用与后台执行；独立的客户端续接构成新的根请求。
- **API Key 入口限流（API Key Root RPM）**：同一 Principal 在严格滑动 60 秒窗口内可准入的根请求数，覆盖模型推理、Responses 远程压缩与 MCP `tools/call`。同根内部重试与隐藏工具轮次不重复计数；不限代表无本地上限，不代表上游容量无限。
- **避免混用**：并发数上限（Concurrency Limit）、连接数、TPM、RPD。

### 目的地限流（Destination RPM）
同一「上游连接（Provider）+ 上游模型」组合在严格滑动 60 秒内可发出的上游请求数。跨 Route 共享同一目的地的计数，每次实际上游请求（含重试）各计一次。限流不改变目标的健康状态、失败计数或冷却状态。
- **避免混用**：Target 健康状态、Route 限流、并发池。

### 出站代理列表与代理选择（Outbound Proxy List / Proxy Selection）
- **出站代理列表（Outbound Proxy List）**：实例级命名的出站代理配置列表（每条包含可选名称与代理地址，不含绕过规则），供模型 Provider、外部 Web Provider 与产品更新/插件市场按需选用。
- **代理选择（Proxy Selection）**：单个上游连接或产品更新的出站代理策略，可选「不使用代理」「使用系统代理」（新建默认值；Desktop 动态读取操作系统代理配置，Server 读取进程代理环境变量，未配置时自动直连）或代理列表中的指定条目。
- **避免混用**：全局强制代理开关、绕过地址规则。

---

## 3. 供应商、插件与模型清单

### 模型厂商与规范模型（Lab / Canonical Model）
- **模型厂商（Lab）**：模型的原生作者身份，也是规范模型 ID 的命名空间（如 `anthropic`、`openai`），区别于提供接入渠道的 Provider。
- **规范模型（Canonical Model）**：独立于具体 Provider 的模型身份与共享规格，ID 固定为 `{lab_id}/{model_id}`；多个目录条目可指向同一规范模型。
- **避免混用**：把 Lab 与 Provider/Vendor 混为一谈；把规范模型当作客户端调用的 Route。

### 供应商目录条目（Provider Catalog Entry）
共享模型目录中某个供应商对单个上游模型 ID 的完整供应模板（合并了规范模型共享规格与该渠道覆盖值）。它属于参考模板，不隶属于任何已保存的 Provider 连接实例。
- **避免混用**：已保存的 Provider Model、Catalog Model。

### 供应商与供应商插件（Vendor / Vendor Plugin）
- **供应商（Vendor）**：具有稳定身份的上游接入实现（可含多个 channel）。内置基础 Vendor 覆盖通用兼容 Profile；专属 Vendor 完整接管对应供应商的全部连接，失败或缺失时永不回退到基础包。
- **供应商插件（Vendor Plugin）**：实现一个 Vendor 的版本化 Wasm Component 软件包，可提供模型推理、完整联网搜索、媒体生成或额度读取中的一种或多种能力。
- **避免混用**：上游连接（Provider）、协议编解码器（Protocol Codec）、npm package。

### 协议编解码器（Protocol Codec）
在规范化模型语义与某种具体线协议（wire protocol）之间做编解码的契约及实现，可被多个供应商插件复用。复用同一编解码器不代表属于同一 Vendor。
- **避免混用**：Vendor、Vendor Plugin。

### 插件源、插件目录与安装来源（Plugin Source / Plugin Catalog / Install Origin）
- **插件源（Plugin Source）**：发布插件的公开 GitHub 仓库及管理员确认的签名公钥，以不可变数字仓库 ID 识别。
- **插件目录（Plugin Catalog）**：Release 中经发布者签名的版本清单，声明产物摘要与宿主兼容范围；签名有效证明来源真实，不等于代码安全。
- **安装来源（Install Origin）**：一次插件安装的实际来源快照（内置、本地导入或插件市场），决定后续更新追踪目标。

### 上游连接（Provider）
管理员保存的一条上游服务连接记录（以 UUID 区分），绑定一个 Vendor 及其 channel、协议、服务地址、常规选项与连接范围凭据。同一 Vendor 可以保存多条独立连接。
- **避免混用**：Vendor（代码实现）、目录条目、客户端 Route。

### 适配器凭据与凭据授权（Adapter Credentials / Vendor Credential Authorization）
- **适配器凭据**：一条 Provider 上由对应 Vendor 声明的多字段上游凭据（区别于客户端 Stravia API Key 与 OAuth 凭据）。
- **供应商凭据授权**：管理员为某条连接登录或填写凭据时，授予选定插件处理该连接上游秘密的信任边界；仅限本连接或待配置认证会话，不授予其他连接或平台凭据访问权。
- **避免混用**：凭据对插件不可见、全实例账户权限。

### 凭据失效（Credential Invalid）
上游明确拒绝某条 Provider 当前凭据组合后写下的持久化健康标记。失效 Provider 下所有目标退出新请求调度（已开始的请求不中断），直到修改非展示字段、OAuth 刷新/重绑成功或手动测试成功才撤销。它与管理员禁用（`is_enabled`）和自动恢复的目标冷却正交；403 权限不足不构成凭据失效。
- **避免混用**：禁用 Provider、目标冷却、熔断。

### 出站形态与槽位头（Egress Shape / Slot Header）
- **出站形态（Egress Shape）**：由宿主管理的连接层参数（TLS ClientHello、ALPN、HTTP/1/2 及 WebSocket 传输特征），不含请求头、凭据或业务内容。
- **槽位头（Slot Header）**：宿主在插件声明位置填入协议值的请求头，表达字段位置与协议意图，不授予插件覆盖宿主授权或连接凭据的权限。
- **避免混用**：供应商 Profile、任意请求头覆盖、User-Agent 设置。

### 供应商私有状态与会话状态（Vendor Private State / Vendor Conversation State）
- **供应商私有状态**：插件为单条 Provider 持久保存的私有数据（如设备注册标识、账号绑定信息），不替代连接配置或模型快照，也不代表网络连接可跨重启恢复。
- **供应商会话状态**：插件在获准的 Provider、Principal 与会话分支内维护的原生请求状态，随确定的历史快照继承，不在分支间共享可变进度。
- **避免混用**：凭据存储、全局插件缓存、共享 Session head。

### 供应商模型、规格与快照状态（Provider Model / Model Specification / Model Snapshot State）
- **供应商模型（Provider Model）**：归属某条已保存 Provider、以上游模型 ID 标识的持久化模型快照，可表示纯媒体生成模型，不以具备对话能力为前提。
- **模型规格（Model Specification）**：模型记录中已登记的上下文长度、输入输出模态、推理强度支持列表与结构化价格；未登记模态时默认文本入/出，其余缺失保持未知，不代表实测承诺。
- **模型快照状态**：记录规格来源以及哪些字段已被管理员按字段固定；同步只刷新未固定字段，固定字段直至恢复同步才跟随来源更新。
- **避免混用**：共享目录条目、Route、实测可用保证。

### 模型清单同步、提供状态与变更记录（Provider Model Sync / Model Change Record）
- **模型清单同步**：检查并协调某条 Provider 供应模型清单的操作；发现新模型不会自动暴露为客户端 Route。
- **不再提供（No Longer Offered）**：曾在清单中的模型在一次完整成功同步中不再出现（不同于管理员手动禁用，也不同于上游标记 `deprecated` 的**上游弃用**）。
- **选择策略（Selection Policy）**：管理员对单个 Provider Model 的路由与发送策略（自动跟随、强制可用、强制禁用）。
- **有效可用性（Effective Availability）**：综合选择策略、清单在场状态与上游生命周期计算出的可用判定，同时约束新路由选择与已有目标的后续发送。
- **模型变更记录**：单条 Provider 的同步结果、手动添加与策略变更的时间线（保留 90 天，跟随连接 UUID），记录 Stravia 确认结果的时间而非上游发布时间。
- **避免混用**：把「不再提供」「上游弃用」「手动禁用」混为一谈；把首次同步时间当成模型发布时间。

### 供应商额度体系（Provider Allowance）
- **供应商额度（Provider Allowance）**：上游报告的当前账户可消费额度快照（订阅配额窗口、请求额度或账户余额），不是 Stravia 统计的本地用量或成本。
- **额度监控器（Provider Allowance Monitor）**：插件提供的受控额度读取与规范化能力。
- **额度条目（Allowance Item）**：快照中单条账户级可消费额度（模型级额度不纳入）。
- **额度状态（Allowance Condition）**：由剩余/已用比例派生的展示态（已用 ≥ 100% 或剩余 ≤ 0 为耗尽；剩余 < 20% 为紧张；其余为正常），本身不直接改变路由。
- **守护条目（Guarded Allowance Item）**：管理员选定用于触发路由保护的账户级额度条目（实例级生效）。
- **额度暂停（Allowance Suspension）**：最近一次成功读取显示任一守护条目耗尽时，该 Provider 下所有目标退出新选路（已开始请求不中断）；直到新的成功读取恢复或取消守护才解除，与禁用、凭据失效和目标冷却正交。
- **额度样本与耗尽预测（Allowance Sample / Exhaustion Forecast）**：每次成功读取留存的时序点（保留 14 天），用于对当前重置窗口做线性外推，预测是否会在重置前耗尽。
- **避免混用**：本地用量统计（Stats Usage）、计费消费上限、目标冷却。

---

## 4. 路由、调度与容错

### 路由、路由 ID 与展示名（Route / Route ID / Model Display Name）
- **路由（Route）**：将对外暴露的逻辑模型名映射到一组路由目标（Target）及其选择策略，至少需包含一个已启用目标。
- **路由 ID（Route ID）**：客户端请求体中填写的 `model` 字段，精确区分大小写，是调用身份而非数据库主键。
- **展示名（Model Display Name）**：给人看的可选、可重复标签，为空时回退显示路由 ID，不参与路由或授权。
- **避免混用**：把展示名当调用键、用数据库主键兜底查找。

### 路由用途（Route Purpose）
一条路由执行的单一任务类型：**对话推理**、**图片生成**或**联网搜索**。同一个 Provider Model 可以具备多种能力，但每条 Route 只服务一种用途。
- **避免混用**：模型全部能力、路由多选模态。

### 路由目标与启停状态（Target / Enabled Target / Disabled Target）
- **路由目标（Target）**：Route 上绑定的一个具体上游目的地（绑定一条 Provider，并在能力需要时绑定上游模型 ID）。
- **已启用 / 已禁用目标**：已启用目标参与选路、亲和与冷却；已禁用目标仍保留在路由配置中，但不参与调度，也不等于已删除。
- **避免混用**：把 Provider 直接等同于 Target；把禁用目标当成已删除目标。

### 目标优先级与路由调度策略（Target Priority / Route Scheduling Strategy）
- **目标优先级（Target Priority）**：有符号 32 位整数（默认 `0`，数值越大越优先），同数值的已启用目标组成同一优先级泳道。它不是列表顺序或权重。
- **路由调度策略**：同一优先级泳道内选路的策略：
  - **流量均衡（Traffic Equalization，默认）**：先选在途请求数最少的，再选近一小时加权 token 流量最低的（均衡流量而非美元成本）。
  - **延迟偏好（Latency Preference）**：综合上游近期输出速度与成功率择优。
- **避免混用**：列表序号、成本均衡、简单轮询（round-robin）。

### 会话亲和与缓存亲和（Conversation Affinity / Cache Affinity）
- **会话亲和（Conversation Affinity）**：请求携带生成链父节点或提示词缓存路由键时，优先命中同一身份上次成功的目标（按 Principal 与 Route 隔离，可压过优先级，不形成 Session）。
- **缓存亲和（Cache Affinity）**：会话亲和未生效时，按前缀内容项摘要（Canonical Item Hash）优先命中曾成功处理该前缀且输入规模达阈值（≥ 20,000 `prompt_tokens`）的目标，以提升上游提示词缓存复用率。
- **提示词缓存（Prompt Cache）与缓存指令（Prompt Cache Directive / Cache Breakpoint）**：上游服务对已计算输入前缀的复用能力；调用方可附带缓存断点、保留期或路由键提示，它不属于模型可见历史，也不阻碍目标续接。
- **避免混用**：Session Affinity、复写历史文本前缀。

### 首 Token、首 Token 超时、重试预算与目标冷却
- **首 Token（First Token）**：当前目标本次尝试从上游收到的第一个规范化输出（含思考内容），早于或独立于客户端可见输出。
- **首 Token 超时（First Token Timeout）**：流式推理在收到首 Token 前允许的连续上游空闲窗口（默认 120 秒 ±5%，收到数据或心跳即续期，0 表示关闭）；超时按瞬时上游失败处理。
- **目标重试预算（Target Retry Budget）**：目标在正常状态下允许的连续上游失败次数（默认 5 次，第 6 次触发冷却），跨同目标重试与跨请求终态上游失败共享计数，完整成功即清零。
- **目标冷却（Target Cooldown）**：目标连续失败超预算或收到长 `Retry-After` 后暂时退出新调度的恢复期（默认 120 秒）；到期后先放行单个请求做半开探测，成功后恢复。
- **客户端输出提交（Client Output Commit）**：响应输出首次不可逆地发往客户端的时点。此前遇到可重试上游失败允许切换目标；此后禁止切换，只能终止当前请求。
- **避免混用**：请求总超时、单次请求重试次数、单次失败即熔断、响应已完成。

---

## 5. 协议、推理与上下文历史

### 推理运行、模型轮次与模型腿（Inference Run / Model Turn / Model Leg）
- **推理运行（Inference Run）**：处理一次客户端生成请求直到交付完整响应或终止的完整过程，内部可含多次隐藏模型轮次与平台工具执行；一旦向客户端交接客户端工具调用，本次运行即告结束。
- **模型轮次（Model Turn）**：一次规范化模型交互（可含同路由内的上游重试，不含平台工具执行）；工具结果触发的下一次调用属于新的模型轮次。
- **模型腿（Model Leg）**：一次推理运行内消费单个模型轮次的完整流转（从 Hook 介入、事件归并到收口及决定公开交付或隐藏续跑）。
- **避免混用**：把单次 HTTP 上游尝试、模型轮次与整次推理运行混用。

### 规范化内容项与有效模型请求（Canonical Item / Effective Model Request）
- **规范化内容项（Canonical Item）**：规范化交互中具有独立身份与语义边界的有序内容单元（文本、思考、工具调用、工具结果等）；流式增量归属同一内容项。内容项 ID、Tool Call ID 与引用三者不可互换。
- **有效模型请求（Effective Model Request）**：完成历史恢复、继承状态解析与 Hook 改写后，准备交给某个目标的完整规范化任务语义，区别于客户端原始线协议报文。
- **避免混用**：原始请求（Raw / Wire Request）。

### 协议转换与表达能力（Protocol Conversion / Representability）
- **协议转换**：在客户端线协议与上游线协议之间转换规范化语义，不负责把响应投影成客户端历史。
- **表达能力（Representability）**：某段规范化语义能否在目标协议中等价表达。目标协议可表达的保留，无法表达的默认省略（不伪造参数或宣称跨协议无损），但不豁免源协议解析或安全校验错误。
- **避免混用**：无损跨协议转换、客户端投影。

### Open Responses 协议与命名空间扩展（Open Responses Protocol / Namespaced Extension）
- **Open Responses 协议**：Stravia 以官方日期版本（`2026-04-24`）为基准的多供应商 Responses 线协议契约，不等同于单一厂商的滚动私有协议。
- **命名空间扩展**：核心契约之外由平台或供应商明确拥有的可选能力，Stravia 自有扩展以 `stravia:*` 标识。

### 钩子与上下文改写（Hook / Context Rewrite）
- **钩子（Hook）**：由平台管理员配置的进程内推理扩展，读取规范化请求/响应语义并以受限动作改写、拒绝或提前完成推理，不接触客户端或上游凭据。
- **上下文改写（Context Rewrite）**：Hook 对一段连续历史所做的稳定语义替换，历史追加后仍依据原始稳定身份持续生效。

### 思考强度控制体系（Thinking Level / Map / Summary）
- **思考等级（Thinking Level）**：平台统一的规范思考强度档位（`off`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`），不是任何单一协议的原生字段名。
- **思考等级映射（Thinking Level Map）**：单个目标上从规范思考等级到上游努力度（Effort）或显式隐藏（Hidden）的对照表，每行分为目录推导（Generated）或人工覆盖（Overridden）。旧型号预算与开关形态仅在出口协议层翻译，不进入核心映射。未精确命中时先向上取最近可用档，无更高档再向下取。
- **支持思考等级（Supported Thinking Levels）**：路由下全部已启用目标非隐藏映射的并集，仅供展示与客户端配置导出，不是运行时强制钳制集合。
- **默认思考等级（Default Thinking Level）**：客户端请求完全未给推理控制时生效的路由级默认规范档位；客户端任何显式推理指令优先于它。
- **思考摘要（Thinking Summary）**：上游返回的可公开推理概述，独立于思考强度控制，也不等同于完整内部推理或加密签名。
- **避免混用**：把原生 `reasoning_effort` 当规范档位；把关闭摘要当成关闭思考。

### 客户端投影、隐藏历史与思考回放（Client Projection / History Marker / Thinking Replay）
- **客户端投影（Client Projection）**：把规范化模型响应转换为客户端可见形态的过程——将平台工具调用/结果与权威思考块替换为**历史标记**，同时保证普通正文流式直出，不因潜在工具调用整段缓冲。
- **隐藏历史片段（Hidden History Segment）**：客户端投影省略但属于模型有效上下文的受保护单元（单个权威思考块，或单次平台工具调用及实际交付结果）。
- **历史标记（History Marker）**：嵌入客户端响应中的 Principal 级不透明引用，用于在后续请求原位恢复对应隐藏历史片段（一对一绑定，不聚合多个工具或思考块）。
- **投影定界符（Projection Delimiter）**：成对无状态机器语法，标记某段正文为展示目的临时借用了其他载体（如正后思考预览），客户端原样回传时按绑定标记恢复权威语义。
- **正后思考（Post-Text Thinking）与引用预览**：客户端收到首个非空正文之后出现的思考内容；在 OpenAI 兼容协议下以 Markdown 引用块展示可公开预览，并由历史标记恢复权威内容。
- **思考回放（Thinking Replay）**：面向当前目标构造的历史推理回放视图——优先乐观回放受保护载荷，遇上游拒绝后仅剥离非本上游来源并按内存拒绝记忆收敛，不改写原始权威历史。
- **避免混用**：把客户端投影当成协议转换；把历史标记当成占位文案或会话 Token；宣称跨模型无损思维链。

### 生成链、响应链、目标续接与原生压缩（Generation Chain / Target Continuation）
- **生成链（Generation Chain）**：归属同一 Principal、由已完整交付（`completed` 或 `incomplete`）的生成请求与响应组成的不可变有向无环历史图（失败、取消或断线不入链），支持分支。
- **自动父节点发现（Automatic Parent Discovery）**：请求未显式指定父节点时，在同 Principal 内按严格规范化历史前缀匹配最长父节点的规则；不凭连接或时间接近猜测。
- **响应链（Response Chain）**：生成链面向 Responses 协议、以平台生成的 response ID 连接的投影。
- **上游存储提示（Upstream Store Hint）**：Open Responses 的 `store` 字段，仅约束上游是否保留自身状态，不影响 Stravia 本地记录生成链。
- **目标续接（Target Continuation）与可复用响应前缀（Reusable Response Prefix）**：上游自身持有、以上游 response ID 引用的状态。当有效模型请求的完整前缀与某已交付节点精确匹配且对应上游状态可用时，优先续接而非重放全量历史。
- **原生压缩边界（Native Compaction Boundary）**：由支持原生压缩的上游（当前仅限 Responses 原生状态）证明的不可变上下文替换点，后续执行使用压缩后的窗口而非旧窗口。
- **避免混用**：把生成链当成可变 Session head；把上游 response ID 混同于平台响应链 ID。

---

## 6. 内置 Agent、平台工具与文件资产

### 内置 Agent 体系（Agent Definition / Runner / Turn Chain）
- **Agent 定义（Agent Definition）与修订版（Revision）**：由平台程序内置并版本化的稳定智能体用例（规定输入输出契约、指令、工具与预算），管理员不能自定义或改写逻辑；**Agent 配置（Config）**仅管理启用状态与绑定的逻辑路由。
- **Agent 执行器（Agent Runner） / 运行（Run） / 轮次（Turn）**：在时间、轮次、token 与工具硬预算内推进有界循环（禁止 Agent 嵌套调用 Agent）；仅成功或合规受预算结束的运行提交不可变的 **Agent Turn**。
- **轮次链（Turn Chain）与轮次引用（Turn Reference）**：归属同一 Principal 的不可变 Agent 历史父链，通过不透明引用 `stravia://turns/<turn-id>` 精确选择续接前缀或分支。
- **避免混用**：自定义工作流搭建器、Session ID、可变 latest head。

### 平台工具、客户端工具与执行交接
- **平台工具（Platform Tool）**：由 Stravia 注册并执行的工具，调用参数与原始结果仅送回模型而不暴露给客户端。
- **客户端工具（Client Tool）与等待状态（Client Tool Waiting）**：由接入客户端自己执行的工具；平台交接调用后本轮运行结束，观测中的「等待客户端工具」仅表示尚未收到回传，不证明客户端执行成败或超时。
- **工具执行交接（Tool Execution Handoff）与工具执行引用（Tool Execution Reference）**：带同步等待窗口的平台工具在等待耗尽后转为独立后台执行的持久确认（交接后不因原请求断线而取消），并交付 `stravia://tool-executions/<id>` 供模型后续经 `StraviaRead` 查询状态与结果。
- **避免混用**：把工具交接当成执行超时或失败；把执行引用当成模型生成的 Tool Call ID。

### 统一读取工具与读取快照（StraviaRead / Read Snapshot）
- **StraviaRead**：客户端、外层模型与内部 Agent 共用的统一内容读取入口，通过单个 `path`（支持 `:` 选择器）加可选 `body` 读取文件资产、网页、搜索、多模态理解、历史回看及后台工具执行结果。外层公开调用与内部 Agent 基础检索保持独立执行身份。
- **读取快照（Read Snapshot）**：归属 Principal 的不可变只读文本视图，支持分页续读已获取内容，不触发重新抓取或生成新搜索轮次。
- **避免混用**：文件存储（Artifact Store）、可重新抓取的页面缓存。

### 文件资产与传输授权（Artifact / Grants）
- **文件资产（Artifact）**：归属同一 Principal 的不可变媒体或大对象，按「媒体类型 + 完整内容」去重，通过不透明 **资产引用（Artifact Reference）** `stravia://artifacts/<artifact-id>` 跨对话复用（仅限同 Principal 解析）。
- **下载授权（Artifact Download Grant）**：针对单个资产的限时签名下载链接，有效期内无须 API Key 即可下载，不代表资产公开或永久有效。
- **上传授权（Artifact Upload Grant）**：允许向指定 Principal 上传文件的临时受限凭据，不授予模型调用、读取或管理权限。
- **避免混用**：把资产引用当成公开 URL 或下载凭据；把上传授权当成 Stravia API Key。

### 高级能力、平台能力门禁与透明注入
- **高级能力（Advanced Capability）**：面向用户提供、受平台总开关控制的能力，包括联网搜索（含网页读取）、多模态理解、媒体生成等。
- **平台能力门禁（Platform Capability Gate）**：实例级能力总开关。关闭时任何 API Key 都不能通过显式调用、MCP 或透明注入使用该能力。
- **透明注入（Transparent Injection）**：客户端未显式声明工具时，平台按 API Key 的已选集合自动注入工具的偏好设置（不是高级能力授权，也不限制显式调用或 MCP；唯独历史回看由 Key 侧配置决定在推理请求中的可用性）。总开关关闭期间，已选择的注入项保留但不生效，重新打开后恢复。商业版扩展可通过 `DynamicPlatformToolLoader` 扩展外部工具透明注入。
- **避免混用**：把透明注入开关当成 API Key 的能力权限开关。

### 联网搜索、网页读取与外部 Web Provider
- **联网搜索（Web Search）**：将查询转为带来源校验的 **搜索报告（Search Report）** 的能力，提交不可变的 **搜索轮次（Search Turn）**，每个经核验的引用条目为 **搜索来源（Search Source）**（`stravia://turns/<id>/sources/<ordinal>`）。
- **搜索后端（Search Backend）**：分为 **Local Agent**（使用外部 Web Provider 提供的内部搜索/抓取叶子与研究预算，支持续接）与 **External Route**（由具备完整搜索能力的供应商插件路由单次执行，不支持续接），两者互不自动回退。
- **网页读取（Web Fetch）**：外层模型经 `StraviaRead` 读取指定公网 URL 并提取正文（**Fetched Page**）的能力，与联网搜索共用平台门禁与透明注入项。
- **Web Access 与 Web Provider**：管理员配置的内部联网来源层，由宿主通过扩展缝（`WebProviderFactory`）注入实现；开源核心默认不内置本地搜索/浏览器适配器，外部搜索路由仍可通过供应商插件使用。
- **避免混用**：把普通网页读取当成完整研究搜索；把底层 Web Provider 当成模型路由 Target。

### 多模态理解与媒体生成（Media Understanding / Media Generation）
- **多模态理解（Media Understanding）**：针对图片（JPEG/PNG/WebP）或 Office 文档回答开放问题的能力，返回带引用校验的 **多模态报告（Media Report）**，提交不可变的 **理解轮次（Media Understanding Turn）**；为源资产生成并复用的规范化图或文档提取清单称为 **媒体派生物（Media Derivative）**。
- **媒体生成（Media Generation）与图片生成绑定**：按提示词及可选参考图生成新媒体 Artifact 的能力。管理面通过 **图片生成绑定** 引用一条「图片生成」用途的 Route（解除绑定前不得修改该路由用途）。
- **避免混用**：把多模态理解局限称为 OCR；把图片生成与多模态理解混淆；把「图片生成绑定」混同于 Route 内的 Target 绑定。

### 历史回看与交互转录（History Recall / Interaction Transcript）
- **历史回看（History Recall）**：允许开启该透明注入项的 API Key 通过 `StraviaRead` 的 `stravia://history/` 路径，只读浏览、读取与检索同一 Principal 在保留期内的交互链路。默认关闭，绝不经 MCP 提供；共用同一 Key 的人可见彼此对话。
- **交互转录（Interaction Transcript）**：单次交互在历史回看中按观测事件渲染的只追加只读文本文件，已完成交互的行号稳定，用于检索结果定位；它不是调试抓包，也不用于重建模型上下文。
- **避免混用**：Session 存储、长期记忆（Memory）、控制台对话的浏览器本地历史、调试包。

### 内置 MCP 与商业版集中式 MCP 管控边界（Built-in MCP / Commercial MCP Hub）
- **内置 MCP（Built-in MCP，社区版 CE）**：社区版仅通过统一 `/mcp` 端点暴露平台内置工具（如 `StraviaRead`、`generate`、联网搜索等），按 API Key 的 `mcp_access_enabled` 总开关控制可见性；不提供外部 MCP 接入、HTTP 转 MCP 桥接、分组编排或细粒度作用域子路径（`/mcp/category/*`、`/mcp/server/*`、`/mcp/group/*` 返回 404）。
- **集中式 MCP 管控与 HTTP 转 MCP 桥接（商业版 `StraviaPlatformEE` Pro / Enterprise）**：通过社区版暴露的 `McpCatalogProvider`、`DynamicPlatformToolLoader`、`GatewayExtensionFactory`、`HttpAppExtensions` 与 WebUI 扩展点在商业版仓库中实现并受离线 License capability（`mcp_external`、`mcp_http_bridge`）门禁控制，提供外部 MCP（Streamable HTTP）接入与透明注入、HTTP 转 MCP 桥接（含 Artifact 附件上传与纯文本绑定）、自定义分组、多作用域 `/mcp*` 入口及独立的 API Key MCP 策略表。
- **避免混用**：MCP Provider、插件、Agent Definition、Route、Webhook、模型绑定、Platform Capability Gate。

---

## 7. 凭据保护（可逆脱敏）

### 可逆脱敏与总开关（Reversible Redaction / Master Toggle）
- **可逆脱敏**：在发往模型的 **受保护模型文本**（系统指令、消息、历史及工具入/出参，不含图片或二进制附件）中，通过本地内置规则与自定义规则识别凭据类秘密并替换为占位符，在模型返回的回答与工具调用参数中再还原为真值。仅限规则可识别的凭据，不覆盖通用个人隐私或业务机密；检测或存储异常时直接失败（fail-closed），绝不静默泄露明文。
- **可逆脱敏总开关**：实例级开关，默认关闭；开启后对全部有效 API Key 生效，无 Key 级豁免。关闭后不再新建脱敏映射，但已有未过期占位符仍继续还原。
- **避免混用**：日志脱敏、永久脱敏、加密、通用敏感信息识别。

### 可逆脱敏映射与凭据标记（Redaction Mapping / Redaction Marker）
- **可逆脱敏映射**：归属单个 API Key（Principal）的「凭据原文 ↔ 占位符」持久化映射，跨对话与重启复用同一占位符，按最近一次规则检出时间计算过期；不同 API Key 间严格隔离。
- **凭据标记（Redaction Marker）**：代表受保护凭据的不透明占位符，可嵌入文本或路径中，由平台在回答与工具参数中按当前 Principal 还原；它不引用隐藏历史。
- **避免混用**：实例级共享字典、History Marker、工具授权。

### 命中记录、映射复用与密钥匹配测试
- **命中记录（Hit Record）**：某 API Key 下每次**新增脱敏映射**时记下的一条绑定记录（创建时间、规则、来源），不因原请求失败或取消消失。
- **映射复用命中**：同一 API Key 下，后续新提交文本再次命中已有映射、或模型在工具参数中复用占位符并被还原的观测记录（重放旧历史不重复计为复用）。
- **密钥匹配测试**：管理员输入样本文本试跑检测规则的工具；即使总开关关闭也可使用，不落盘样本、不建立映射，也不验证凭据真实有效性。
- **避免混用**：把「新增映射」当成「每次规则命中」；把匹配测试当成密钥有效性校验。

---

## 8. 交互观测、调试、用量与计费

### 客户端交互、交互观测与交互链路
- **客户端交互（Connect Client Interaction）**：归属同一 Principal、由一次或多次推理运行推进的同一次客户端任务交互（可跨越多个生成链根）；诊断归并不改变模型输入或权限。
- **交互观测（Interaction Observation）**：面向管理面的持久化诊断投影，记录交互、推理运行、模型轮次、工具调用、目标尝试与用量，也是历史回看的唯一只读来源。观测失败或丢失绝不影响推理执行。
- **交互链路（Interaction Chains）**：按诊断父边组织的交互因果树，供请求记录页与历史回看共用。交付过任何客户端可见输出的交互入链；零输出且最终失败的交互不入链，归入客户端错误视图。
- **被拒请求观测（Rejected Request Observation）**：请求在形成推理运行前即因解码、协议或认证错误被拒绝时留下的独立诊断记录，不归属任何 Principal 或交互链路。
- **诊断续接判定（Current Tool Continuation / Retained Tail Association）**：观测层用于跨生成链根归并同一交互的两种诊断规则（优先按待完成工具结果精确归并，其次按唯一幸存尾段推断），不影响模型执行历史。
- **避免混用**：把交互观测当成模型历史事实源（Generation Chain）；把交互链路当成 Session。

### 请求终态分类与成功率指标
- **错误的请求（Rejected Request）**：发出任何上游请求前被平台校验拒绝的请求（鉴权失败、模型不存在/无权访问、Hook 拒绝、参数无效等）。不计入请求成功率分母。
- **失败的请求（Failed Request）**：最终因平台或上游故障失败的请求（无可用目标、额度暂停、超时、上游连接或流式错误等）；重试/切换后最终成功、客户端主动取消/断线以及错误的请求均不算此类。
- **客户端错误（Client Errors）**：观测中按客户端视角汇总「失败的请求」与「错误的请求」的列表视图（不等同于 HTTP 4xx）。
- **上游错误（Upstream Error）**：单次实际发往上游的目标尝试收到的上游终态失败（非 2xx、传输/流错误、首 token 超时、凭据被拒），无论是否被重试吸收均独立计次。
- **最终成功率（Final Success Rate）**：最终正常完成的请求 ÷ 已终结请求（完成 + 失败的请求；不含取消、断线、进行中与错误的请求）。
- **一次通过率（First-pass Success Rate）**：未依赖失败后自动重试或切换目标便完成的请求 ÷ 同一分母。
- **自动恢复成功的请求（Recovered Request）**：单次请求中经历自动重试或路由切换后最终完成的请求（多次恢复只计一次），其占比即最终成功率相对一次通过率的百分点提升。
- **上游调用成功率（Upstream Attempt Success Rate）**：`1 - 上游错误 ÷ 已获上游判定的尝试总数`，反映上游自身稳定性，不能直接与请求级成功率相减。
- **避免混用**：把「错误的请求」与「失败的请求」混称；把「客户端错误」等同于 HTTP 4xx；把「上游成功率」混作「最终成功率」。

### 线级调试抓包与交互调试包
- **线级调试抓包（Wire Debug Capture）**：覆盖客户端↔Stravia↔上游四个方向的应用协议级诊断记录（HTTP 字段、body、SSE、WebSocket 消息与时序），不是底层 TCP/TLS 抓包；仅永久遮盖 HTTP `Authorization` 头值，其他凭据、正文与媒体可能保留。
- **推理运行调试追踪（Inference Run Debug Trace）与清单（Manifest）**：由进程级 Debug 开关（重启不保持）控制生成的单次运行四方向抓包及完整性说明。普通观测在 Debug 关闭时亦采用相同的 `Authorization`-only 过滤标准，但不做完整抓包。
- **交互调试包（Interaction Debug Bundle）**：管理员导出的版本化 ZIP 诊断包，汇总某次交互当时已落盘的调试追踪并明确标示未捕获或不完整的运行；应按高度敏感数据妥善保管。
- **避免混用**：网络抓包（Packet Capture）、完整备份、普通请求观测。

### 用量、成本与计费
- **已确认上游用量（Confirmed Upstream Usage）**：上游在模型调用中明确报告并被 Stravia 收到的 token 用量；未报告部分保持未知，绝不记零或本地估算冒充。
- **总输入 Token（Total Input Tokens）与净输入 Token（Net Input Tokens）**：前者为上游报告的原始含缓存输入，用于原始记录、协议响应与计价；后者为总输入扣除缓存读取后的非负值，是管理面「输入」指标的统一口径。
- **用量统计汇总（Stats Usage）**：独立于对话历史与观测记录长期保留的聚合指标（Token、成本、计费、请求与错误计数），不因清理历史而减少；API Key、Route 或 Provider 删除后保留各自历史统计身份，同名重建不合并。
- **自定义币种（Custom Currency）**：美元之外管理员配置的唯一展示币种（符号 + 汇率）。每笔成本与计费发生时同时冻结美元与自定义币种金额；改符号不换算，改汇率只影响后续记录。
- **估算成本（Estimated Cost）与按量成本（Usage-Based Cost）**：依据本地登记的上游价格（含上下文分档与**峰谷定价 Peak Pricing** 时段倍率）或订阅费，对本实例应向上游支付金额的本地估计；不是上游官方账单。
- **计费（Billing）与消费上限（Spend Limit）**：按 Route 计费价格对 API Key 发起的调用做的内部记账；累计达到消费上限后拒绝新请求但不禁用 Key，调高上限或清零已消费后立即恢复。
- **避免混用**：把估算成本当成计费或上游账单；把消费上限混同于上游额度（Provider Allowance）；把净输入当成总输入。
