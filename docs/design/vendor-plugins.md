# Vendor Wasm 插件设计

本文记录 Vendor Wasm 插件的已确认边界与实现契约。运行代码、构建入口与行为验证共同作为事实来源，未确认的策略不得作为实施依据。

## 已确认的身份与信任

- 一个 Vendor Plugin 软件包实现一个稳定的 Vendor 实现身份；Vendor 身份不由 npm package、Provider Catalog 或协议决定，见 [ADR-0061](../adr/0061-separate-vendor-identity-from-package-and-protocol.md)。
- 最终随程序交付恰好五个 Vendor：`base` 回退 Vendor，以及 `openai-codex`、`xai-grok`、`command-code`、`devin` 四个专属 Vendor。`base` 是一个软件包和一个 Vendor 身份，通过多个 Provider Profile 覆盖四个专属接入之外的全部现有供应商，不是“一个包导出多个 Vendor”。
- Provider Profile 的 `provider_id` 标识供应商接入，`catalog_id` 仅关联目录；两者都不等于已保存 Provider 连接的数据库 UUID。普通 OpenAI 与 xAI API Profile 归 `base`，Codex 与 Grok 分别使用 `openai-codex` 与 `xai-grok` 专属 Profile。
- 既有 `openai/codex` 与 `xai/grok` 连接只迁移供应商 Profile 归属，分别指向 `openai-codex` 与 `xai-grok`；连接 UUID、channel、凭据、Route 和历史保持不变。
- 专属 Profile 对匹配身份下的全部连接、channel 和操作进行整体接管。专属包不存在、缺少能力或 channel、加载失败或执行失败时明确失败；不得按单次能力、channel 或操作回退到 `base`，也不得把两边的声明、网络 origin 或行为合并。
- 管理员为所选插件的连接登录或填写凭据，即授权插件处理该连接的上游秘密；宿主拥有凭据持久化与跨连接隔离，见 [ADR-0062](../adr/0062-trust-vendor-plugins-with-connection-credentials.md)。

### 随附身份映射

| crate | Vendor ID / kind | Provider Profile |
| --- | --- | --- |
| `stravia-vendor-base` | `base` / `fallback` | 除下列四个专属 Profile 外的全部现有供应商；`openai`、`xai` 仅保留普通 API channel。 |
| `stravia-vendor-codex` | `openai-codex` / `dedicated` | `provider_id = openai-codex`、`catalog_id = openai`、channel `codex`。 |
| `stravia-vendor-grok` | `xai-grok` / `dedicated` | `provider_id = xai-grok`、`catalog_id = xai`、channel `grok`。 |
| `stravia-vendor-command-code` | `command-code` / `dedicated` | `provider_id = command-code`、`catalog_id = command-code`、channel `default`。 |
| `stravia-vendor-devin` | `devin` / `dedicated` | `provider_id = devin`、`catalog_id = devin`、channel `devin`。 |

## 已确认的供应商能力覆盖

- 一个供应商 Profile 可以统一提供推理适配、自定义上游编解码、OAuth、自定义模型发现、额度获取与供应商特有计算，以及自定义 Provider 选项声明和校验；拆为五个包不缩减任何既有供应商能力。
- `stravia-vendor-base` 承接 Codex、Grok、Command Code、Devin 之外的全部现有供应商接入，包括 Anthropic OAuth、云认证与云协议、模型发现、额度查询和供应商差异。它在单一 `base` Vendor 身份内按输入 `provider_id` 分派 Profile，不把 Profile 暴露为多个 Vendor。
- 基础包在构建期从 `assets/providers.stravia.json` 生成完整的目录 Profile 静态表，不另维护一份品牌名单，也不在每次 guest 调用中解析整份注册表。兼容品牌复用对应的完整实现族，但不复制其他供应商的 OAuth channel 或认证 origin；运行时刷新目录不会隐式新增可执行身份。
- OpenAI-compatible（包括 embeddings）、Anthropic、Gemini、Open Responses 四类标准 codec 由共享库提供，作为可复用实现源码由五个 guest crate 按需引用。
- DeepSeek Profile 位于 `base`，复用标准 codec 并保留必要的供应商差异与额度能力，不要求为复用标准协议单独维护一套 codec。
- Codex、Grok、Command Code、Devin 使用独立专属插件。Devin 保留自定义 Connect-RPC / protobuf 编解码、OAuth、模型列表与相关元数据、AssignModel 辅助调用及额度获取；Command Code 的自定义 Provider 选项注册属于插件契约，不能依赖宿主的供应商专属前端或后端分支。
- 上述范围覆盖现有能力，不表示每个 Profile 都必须实现每种供应商功能；具体能力由对应 `ProviderDescriptor` 声明并按 Profile 独立校验，不能从同包其他 Profile 合并推断。

### 多能力扩展的已确认范围

- 同一个 Vendor Plugin 可以提供模型推理、完整联网搜索、媒体生成中的任意非空组合；允许只提供完整搜索或媒体生成，不要求伪装成可对话模型。同一 Provider 连接可以供其支持的多种能力复用。
- 本次联网扩展限于 Codex hosted search 一类完整 Web Search 后端，不包含 Local Web Search 使用的基础 search/fetch Provider；完整研究报告与基础检索结果不合并为一种契约。
- 媒体生成沿用现有设计的图片生成与参考图编辑范围，不因插件能力可组合而提前增加音频或视频类型。
- 媒体生成沿用 Provider Model → Target → Route，不另建直接绑定 Provider 的执行路径。Provider Model 可表示纯图片模型；Codex 的 GPT 模型可以同时支持推理与图片生成。
- 媒体生成绑定及执行按所需能力校验；纯图片模型不能被普通聊天请求选中，不以虚构聊天能力满足路由接入要求。继续遵守媒体生成设计中所有已启用 Target 的资格校验规则。
- 完整联网搜索引入 Route 绑定，替代 Codex 专属的固定 Provider 与 upstream Model 绑定。搜索 Target 支持 Provider 加上游模型，也支持仅 Provider 的独立研究服务；后者不要求创建 Provider Model 或虚假模型 ID。Provider-only 契约及运行验证属于本期，未来服务的正式接入不自动成为本期交付要求。
- 独立搜索服务以 [Parallel Responses API](https://docs.parallel.ai/responses-api/responses-quickstart) 一类直接交付带引用综合答案的服务为参考，仍接入完整 Search Report 契约，不扩展为仅返回基础检索结果的来源。这里“不选择模型”指管理员无需选择模型，不表示上游 wire 必须没有 model 字段；供应商要求的固定标识由插件处理，不强迫管理员建立虚假的可选模型。
- 外部完整搜索后端按单次独立调用处理，不提供续接，不跨请求保存可用于续接的上游研究会话；即使供应商具有会话能力，本次也不使用。此决定不取消既有 Local 搜索的续接能力。
- 外部搜索沿用 Route 的选择、重试与 Target 切换策略，由插件提供上游错误分类，宿主决定执行。明确可重试的上游错误可在结果提交前重试或切换；用户取消、参数错误及权限错误不触发。接受可能重复执行与重复消耗额度，不承诺恰好执行一次。
- 更新允许移除已被使用的能力，必须明确展示受影响绑定。保留原绑定并标记能力不可用，执行时明确失败，不自动换账号、改绑或降级为普通推理；数据兼容时已开始的调用仍由旧版本完成。
- 内置自动更新即使移除正在使用的能力，也继续更新，不新增暂停确认步骤；更新后展示受影响绑定及能力不可用状态。这不豁免既有的数据丢弃确认，也不允许跳过宿主契约兼容性检查。
- 宿主保留公开工具、平台能力开关、报告及引用校验、Artifact 收存与归属、历史和用量；插件提供类型明确的供应商能力，不注册新的公开搜索或生成工具。Codex 同连接的推理、hosted search 和托管图片生成纳入统一插件验收。
- 本轮多能力扩展已整体确认，实施与验收以 [完整规格](../../.scratch/vendor-wasm-plugins/spec.md) 为准。

## 已确认的统一运行机制

- 最终所有模型 Vendor 均通过同一套 Wasm 插件机制加载与执行，不长期保留原生 Vendor 通道或供应商专属宿主执行分支。
- `base` 与四个专属 Vendor 共五个自包含 Wasm 包随程序交付，首次使用不要求管理员逐个手动安装；它们同样显示在插件管理页面，并支持本地包替换和恢复内置。
- 宿主仍拥有通用网络、存储、授权、调度及客户端协议处理，不将整个网关移入 Wasm；供应商拆分不改变这些平台职责。
- 预置与本地安装是分发来源的差异，不构成两套供应商能力契约，见 [ADR-0064](../adr/0064-run-all-model-vendors-as-wasm-plugins.md)。

## 已确认的自定义协议范围

- Vendor Plugin 的自定义协议仅用于上游接入，不开放客户端入口注册。
- 插件可以自定义上游请求、响应及流式编解码，包括 Devin Connect-RPC / protobuf，但必须接收与产出宿主规定的 canonical 语义。
- 插件不得注册任意 HTTP 路由、客户端认证方式或 MCP 工具；安装供应商插件不等于对外提供该供应商的原生服务入口。
- 宿主决定对外开放的客户端协议，仍可引用共享标准 codec 库；增加新的客户端协议需要宿主支持，不通过 Vendor Plugin 安装隐式开放。
- 无法表示的任务语义必须显式拒绝，不允许绕过 canonical 契约直接透传客户端流量。

## 已确认的技术栈与插件契约

- 唯一目标技术栈为 Wasmtime + WebAssembly Component Model + WIT，不同时提供 Extism 或另一套自定义 Core Wasm ABI。
- Wasmtime 负责加载、执行与资源限制；`stravia:vendor@0.2.0` WIT 定义插件导出的供应商能力和导入的受控网络、凭据等宿主能力。
- `VendorDescriptor` 包含 `vendor_id`、版本、展示元数据、canonical 格式版本、`kind` 与 `providers`；`kind` 为 `fallback` 或 `dedicated`。每个 `ProviderDescriptor` 独立声明 `provider_id`、可选 `catalog_id`、channel、能力、配置、网络权限和数据兼容信息。
- `fallback` 描述符的 Vendor ID 必须是 `base`，可声明多个互不重复的 Profile；`dedicated` 描述符必须恰有一个 Profile，且 `provider_id` 等于 `vendor_id`。旧描述符形状不保留 alias 或兼容 shim。
- `ProviderSnapshot.provider_id` 在 SDK 与 WIT 中均为必填供应商 Profile ID，不是连接 UUID。运行时按该字段选择唯一 `ProviderDescriptor` 后再做能力、channel 和网络准入，不得合并其他 Profile；`base` guest 据此分派，专属 guest 拒绝其他 ID。
- channel 可通过 `default_models_source: "catalog"` 声明未指定来源时默认使用模型目录；未声明时保留插件自身的发现行为，宿主不以默认值覆盖已保存的来源。该声明只接受目录枚举值，不接受任意 URL，不表示其他 channel 不能选择目录。目录创建入口只有在当前快照包含对应目录时才保存 `catalog` 来源；纯插件声明的入口使用其默认发现方式。宿主不按供应商 ID 猜测默认发现策略。
- `base` 保留既有清单优先级：显式静态模型优先；原生 OpenAI、Anthropic、Google、Ollama、OpenRouter、xAI 的账户发现，以及 Claude Code、Vertex 的渠道精选清单，不被历史 `catalog` 来源标记覆盖。目录别名仍使用自己的目录范围，不能继承另一供应商的账户清单策略。该判定属于 guest，不移回 Core。
- 发现操作与同步富化按 `catalog_id` 提供原始目录 scope，不再次套用目录的旧 channel 定义。只有 `ProviderNotFound` 表示可选目录条目不存在，不提供目录模型快照，由插件决定其发现行为；消费目录来源的插件必须对缺失快照报错，不能返回伪造的空成功。目录访问或解析失败仍向上传播，不触发隐藏回退。
- 已保存的非 `catalog` 发现地址保留完整路径与查询参数，并使用所属供应商的发现认证策略；不能重新拼成推理基址的 `/models`，也不能把所有显式地址一律改为 Bearer。Google 官方原生目录使用 API-key 查询参数，自定义目录保留既有 Bearer 约定；标准 Anthropic 目录保留 `x-api-key` 与版本头。
- `descriptor`、`select-protocol`、`execute` 的导出结构保持不变。首先提供 Rust 插件 SDK，以复用现有供应商实现；WIT 契约不限定插件必须使用 Rust，其他语言的实际工具链兼容性需要验证。
- 发布产物为单个自包含 Wasm Component，携带锁定的 codec 依赖；不直接跨契约暴露 Gateway、数据库对象或 Rust trait 内存布局。
- 宿主与插件通过明确版本的类型和资源交互。Wasmtime 版本以根目录 `Cargo.toml` 与 `Cargo.lock` 为准，guest 工具链以 `rust-toolchain.toml` 为准；技术栈决策见 [ADR-0066](../adr/0066-use-wasmtime-components-and-wit-for-vendor-plugins.md)。
- 推理与原生压缩在执行前调用同一固定版本的 `select-protocol` 导出，由插件根据连接快照与 canonical 请求选择实际上游协议。该阶段只允许纯计算，禁止网络、私有状态和事件副作用，并受相同的取消、截止时间与资源预算约束。宿主随后基于返回协议确定回放身份和语义转换，不维护 OpenAI 等供应商的协议偏好分支。

### 实现与验证入口

- `backend/crates/stravia-vendor-sdk/` 提供 Rust SDK 与 `stravia:vendor@0.2.0` WIT；`stravia-runtime-contract` 提供 canonical 类型，`stravia-protocol-codec` 提供四类标准 codec 与通用 canonical 转换辅助。
- `backend/crates/stravia-vendor-runtime/` 实现 Component 执行与受控资源；Core 的 `src/plugin/` 负责安装、连接快照、网络授权、私有状态及版本切换协调。
- `backend/crates/stravia-vendor-base/`、`stravia-vendor-codex/`、`stravia-vendor-grok/`、`stravia-vendor-command-code/`、`stravia-vendor-devin/` 是五个随附 guest 实现来源。
- `backend/crates/stravia-vendor-common/` 是只提供多个 guest 实际共用辅助代码的 `rlib`。标准 codec 由 guest 在 Rust 源码层链接 `stravia-protocol-codec` 并编入 Component，而非采用 Component composition；Command Code 与 Devin 私有 codec 留在各自 crate，Bedrock、Cohere、Gateway、WatsonX 等私有实现留在 `base`。
- `task build:vendors` 一次构建五个 Component，并生成 `target/vendor-plugins/manifest.json`，安装时不下载依赖。`task build:vendor-fixtures` 构建真实测试组件。Core 的 `vendor_*` 契约与生命周期检查通过 Gateway 和本地上游验证行为；浏览器、Desktop 与双数据库验收仍分别使用根 `Taskfile.yml` 中的对应入口。

## 已确认的管理与分发范围

- 提供插件管理页面，在其中展示已加载的插件。
- 插件管理页面提供本地包安装与更新入口，并展示更新结果，包括随宿主升级发生的内置插件自动更新。
- 除随程序交付的内置插件外，当前仅允许管理员提供本地插件包安装或更新。
- 当前不做插件市场，不提供在线插件获取流程。

## 已确认的产物存储与备份边界

- 已验证的 Wasm Component 作为不可变、内容寻址文件保存在实例 `<data_dir>/plugins/artifacts/<sha256>.wasm`；不把 Component 字节存入 SQLite 或 PostgreSQL，也不在持久化记录中保存任意外部文件路径。
- SQL 只保存摘要、来源、版本、revision、epoch 等安装元数据以及业务数据和插件私有状态。供应商 Profile ID 与连接 UUID 分列保存，不能用 `base` 包身份替代 Profile 归属或把两者误设为同一外键。
- 安装与更新必须先校验、写入并同步产物文件，再提交使新 revision 生效的数据库元数据；任一步失败都保留旧产物与旧安装记录，失败的新 revision 不生效，不能先覆盖旧版本。旧插件若与当前宿主契约不兼容，仍不得继续执行。
- 完整备份与恢复必须同时包含数据库和 `<data_dir>/plugins/artifacts/`。这对 SQLite 与 PostgreSQL 同样成立：只复制 SQLite 数据库文件或只备份远程 PostgreSQL 都不构成完整实例备份。
- 数据目录迁移工具随现有数据目录一起复制 `plugins/artifacts/`，不另增独立插件路径参数；迁移完成前不能把仅有 SQL 元数据的目标实例视为可运行恢复。

## 已确认的配置表单与认证交互

- 插件只声明配置结构，不携带供管理面执行的 JavaScript、HTML 或 Svelte 组件；宿主统一呈现表单，Desktop 与 WebUI 共用配置契约。
- 字段支持布尔、字符串、整数、小数与枚举，声明 key、名称、说明、默认值、必填及范围等约束，并允许必要分组与简单条件显示；不通过表达式脚本实现任意界面逻辑。
- 普通选项与秘密凭据分开。通用字段约束由宿主检查，跨字段业务约束由 Wasm 插件校验；前端展示校验结果，不复制供应商业务规则。
- 支持配置校验的插件可以提出派生地址；内置云供应商仅在地址为空时生成建议，保留管理员显式填写的地址。管理面必须先展示最终地址及其精确 origin，再由管理员确认保存，修改候选配置会使旧审阅失效。OAuth 登录仍要求管理员明确提交非空地址，不把尚未展示的校验建议当作网络授权。
- Command Code 的 zdr 等供应商选项通过同一声明机制呈现、校验和保存，不在宿主前端添加供应商专属表单代码。
- OAuth 使用宿主提供的打开授权链接、输入授权码、等待完成等标准交互；供应商登录网站在浏览器打开，不作为插件自带页面嵌入管理面。
- 此限制不缩减插件对 OAuth 请求构造、token 交换与供应商响应解析的所有权，也不授予插件访问管理会话的能力。

## 已确认的插件私有持久化状态

- 允许插件保存设备注册标识、账户绑定信息等必要的跨重启私有状态，由宿主管理持久化。
- 插件只能访问当前供应商 Profile ID 与 Provider 连接 UUID 共同限定的命名空间；`base` 的多个 Profile 不能共享私有状态。连接尚未保存时，使用对应认证会话的临时状态，不获得其他连接或认证会话的访问权。
- 不开放任意 SQL、数据库表或文件路径，不允许插件自行修改数据库 schema。
- 配置、凭据和模型清单继续使用各自专用契约，不以私有存储绕过其校验、授权与生命周期规则。
- 插件声明私有数据格式版本，供更新兼容性检查使用；不兼容时按已确认的数据丢弃规则由管理员决定。
- 纯性能缓存优先放内存；活跃请求和连接句柄等运行态不能作为可跨重启恢复的持久化状态。
- 私有状态通过 WIT 的 `read-private-state` / `write-private-state` 整体读取或替换，每个 Provider 或认证会话最多保存 256 KiB。存储操作受任务生命周期约束，被取消的旧版本任务不能继续写回，超过容量的替换不能覆盖已保存状态。

## 已确认的供应商流程与网络执行

- 插件允许主动调用宿主提供的受控 HTTP / WebSocket Interface，自行编排供应商流程，不强制通过返回 RequestPlan 让宿主逐步推进。
- 插件拥有供应商端点选择、请求编码、响应解析和辅助调用顺序，覆盖 Devin AssignModel、OAuth、模型发现、额度查询及正式推理。
- 宿主执行实际网络操作，统一应用代理、目标访问限制、取消、deadline 与诊断。插件不能创建绕过宿主的原生 socket。
- 各类操作复用受控网络能力，但权限、凭据范围及资源生命周期绑定对应操作，不能借模型发现或额度查询获得其他连接的访问权。
- 模型请求重试、Target failover 与请求重放决策继续归宿主，插件不得自行重放生成请求；供应商辅助调用编排不意味着拥有平台调度策略。
- WebSocket 失败通过强类型传输事实跨越 WIT 边界，不直接授权重试。只有未提交结果且既有 Route 策略允许同 Target 重试时，宿主才将下一次尝试设为 HTTP-only，并去除仅当前 WebSocket 可用的续接状态，以完整历史重放；下一次客户端请求恢复自动传输选择。
- 插件以 `continuation-not-found` 和 `protected-reasoning-rejected` 报告已确认的续接丢失或受保护推理拒绝，不将普通上游错误归入这些事实。宿主只在输出前且恢复预算允许时执行一次完整历史恢复；受保护推理恢复必须实际移除对应载荷，不向客户端暴露未经脱敏的上游错误文本。
- 同一次 Model Turn 的完整历史恢复、认证刷新及随后重放持续复用最初的组件、操作租约与数据代际，不能在恢复循环中重新取得管理器当前版本。真实 HTTP 401 可在未提交且预算允许时进入一次认证恢复；403 不触发刷新。插件声明的辅助认证刷新也由宿主调度，不要求伪造持久化 OAuth 记录。
- OAuth 回放与原生压缩的账户身份采用宿主持久化的稳定认证连接标识，而不是 access token、refresh token 或到期时间。正常刷新保持身份，重新登录创建新身份；非 OAuth 凭据变更仍改变其指纹。Target、供应商、channel、协议、模型、代理、地址与选项边界保持不变。
- 具体职责取舍见 [ADR-0065](../adr/0065-orchestrate-vendors-through-host-network-capabilities.md)，网络授权采用下述元数据声明与白名单原则。

## 已确认的操作调度与后台执行限制

- 推理、完整搜索、媒体生成、OAuth、模型同步、额度查询与凭据刷新均由宿主创建操作并调用插件，不开放插件独立常驻后台任务。
- 宿主统一决定周期查询、凭据刷新时机，以及插件更新或停用期间的任务暂停和取消。
- 插件可在宿主发起的操作内部编排网络请求、处理流并并发执行必要的辅助调用；所有子任务受该操作的权限、取消和资源生命周期约束。
- 操作结束或取消后，不得遗留继续请求网络或写入存储的插件后台任务。
- 取消或 deadline 可以产生终态错误通知，但不能继续发布成功内容或写回数据。错误通知仍受插件代际撤销约束，慢消费者不能阻塞驱动释放资源。纯协议选择中的副作用、越权 origin 和非法 canonical 输出属于插件执行失败，不归责为客户端参数错误。
- 插件更新引起的取消与 deadline 在搜索、媒体及相应审计中保留强类型归责，不因为调用方 token 未取消就改报上游失败，也不通过取消调用方的全局 token 冒充同一事实。
- 需要跨操作复用的连接池或 WebSocket 由宿主管理，并保持连接与凭据隔离；连接复用不授予插件脱离操作生命周期执行的权力。
- 新的周期性供应商任务需通过明确的宿主能力接入，不允许借私有状态或初始化入口启动无人管理的定时任务。

## 已确认的网络白名单原则

- 插件必须在元数据中声明所需网络访问范围，宿主按白名单策略执行受控 HTTP / WebSocket 请求；未获白名单允许的目标默认拒绝。
- 网络访问范围不得仅隐含在插件代码中，也不得通过直接 socket 或重定向绕过白名单检查。
- 元数据声明是网络权限的输入，不表示插件可自行修改生效白名单。
- 允许访问的目标范围为当前连接由管理员保存的 base URL 所对应的 origin，以及插件声明并获准的 OAuth、模型发现、额度查询等附属服务 origin；不授予任意出站权限。
- 附属 origin 可由插件元数据声明为连接配置中的地址字段，例如 SAP AI Core 的 `tokenUrl`。宿主仅从管理员保存的值解析精确 origin，在保存或变更时展示范围，并将权限固定到操作快照；插件运行时返回值、私有状态和上游响应不能增加授权，不支持通配域名。
- origin 按协议、主机与有效端口匹配，路径由插件构造。HTTP 与 WebSocket 使用各自明确获准的协议，不因主机相同就隐式扩大协议或端口范围。
- 管理员可以显式将当前连接配置为本机或局域网服务，不一刀切禁止私网目标；这一配置不授权访问其他本机或私网地址。
- 安装或更新页面展示插件声明的附属服务地址。每次重定向后的目标也必须重新检查，不自动将原目标的认证头转发到另一 origin。
- 使用出站代理不扩大允许访问的目标范围。允许访问目标与向目标发送凭据是不同的授权，不得根据网络白名单自动注入其他目标的凭据。
- 内置插件随宿主自动更新时，新增声明的网络 origin 无需单独确认，由宿主随受信内置版本更新生效白名单；这不授予插件在运行期间自行修改声明或访问任意目标的权力。
- 本地包手动更新在已有确认界面展示网络范围变化，沿用一次更新确认，不额外增加网络授权步骤。

## 已确认的本地包导入方式

- 管理员在插件管理页面选择本机文件，导入当前连接的 Stravia 实例。
- Desktop 将所选文件导入本机实例；Server WebUI 将浏览器所在机器的文件上传并导入服务器实例，不要求浏览器与服务端位于同一机器。
- 不支持输入远程 URL 下载插件，不自动获取依赖；首期不增加输入服务器文件路径的安装入口。

## 已确认的标准 codec 依赖

- OpenAI-compatible（包括 embeddings）、Anthropic、Gemini、Open Responses 四类标准 codec 由 `stravia-protocol-codec` 共享维护，供应商插件不复制源码维护平行实现。
- 需要标准 codec 的 guest crate 在 Rust 源码层直接链接该库并锁定依赖，构建时静态编入最终交付的一个自包含 Wasm Component；不采用运行时宿主 codec，也不再把 Component composition 保留为未决定路径。
- Command Code 与 Devin 的私有 codec 分别由 `stravia-vendor-command-code` 与 `stravia-vendor-devin` 所有；Bedrock、Cohere、Gateway、WatsonX 等私有 codec 由 `stravia-vendor-base` 所有。`stravia-protocol-codec` 与宿主构建不得反向依赖这些供应商私有实现。
- 安装时无需另行获取 codec 包，运行时不由宿主替换插件内部 codec。每个插件产物携带的 codec 保持固定；随宿主升级自动更新内置插件时，替换的是整个插件产物及其锁定依赖，宿主契约兼容性仍需独立校验。
- codec 修复通过重新构建、发布并更新相关插件交付，接受不同插件产物包含重复 codec 代码的代价，见 [ADR-0063](../adr/0063-bundle-pinned-codecs-in-vendor-plugins.md)。

## 已确认的本地包手动更新流程

1. 管理员选择本地插件包；宿主校验包结构、Vendor 身份及宿主契约兼容性。
2. 页面展示旧版本与新版本、受影响的 Provider，以及新代码将继承这些连接的凭据访问权。包自报的名称、作者或版本不能作为来源已验证的证据；来源未验证时如实说明。
3. 管理员点击“确认更新”，即授权新包接替旧版本并继承受影响连接的凭据访问权，不另设批准步骤。
4. 兼容更新成功后，新调用使用新版本，已开始的调用继续使用原版本，不在流中途替换实现；不兼容更新按下述规则取消该 Vendor 的全部活跃任务。
5. 校验或准备加载失败时保留旧版本可用，不先覆盖旧版本再尝试加载新包。

普通更新确认不自动授权丢弃既有数据；发现数据不兼容时，必须明确展示影响并由管理员确认是否丢弃，遵循下述不兼容更新规则。

## 已确认的内置插件自动更新

- 允许内置插件随宿主升级，自动更新为新版程序携带的更高版本，无需管理员逐个确认插件更新；这是本地包手动确认规则的明确例外。
- 随附版本较低或相同时，不因宿主升级自动覆盖当前版本。
- 自动更新使用程序随附的本地插件产物，不引入插件市场、远程包获取或依赖下载。
- 自动更新同样需要校验与准备加载；失败不先破坏原插件产物，也不代表不兼容的旧插件仍可在新宿主上执行。
- 插件管理页面展示自动更新后的实际版本与结果。该例外不扩展为第三方插件的自动更新授权。
- 自动更新可以同时增加内置插件声明的网络 origin，无需额外确认；管理员对内置更新来源的信任包含其网络声明变化。数据丢弃仍必须按不兼容更新规则手动确认，不因网络扩权免确认而放宽。

## 已确认的本地替换与恢复内置

- 管理员通过本地包替换内置 Vendor 的实现后，该 Vendor 转为手动管理，不再被宿主随附的更高版本自动覆盖。
- 自动更新资格由宿主记录的实际安装来源决定，不采信包自报的“官方”身份，也不因 Vendor ID 相同而恢复。
- 插件管理页面提供“恢复内置版本”操作，展示当前版本、程序随附版本及受影响连接；管理员确认后，使用随附实现并恢复随宿主自动更新。
- 内置来源的插件加载失败或与随附版本不同时，也提供同一恢复入口，无需先导入本地替代包；已有待确认更新时，保留单一的更新审阅入口。
- 恢复内置可能替换为较低版本，必须明确展示；这一显式操作不改变内置自动更新只升级、不降级的规则。数据不兼容时同样需要丢弃数据的明确确认。

## 已确认的不兼容更新原则

- 允许更新为不兼容现有连接数据的插件版本，不将数据向后兼容作为绝对更新门槛。
- 发现不兼容时，由管理员手动确认是否允许丢弃现有数据；不得将普通更新确认或内置自动更新授权视为数据丢弃授权。
- 内置自动更新遇到数据不兼容时暂停切换，在管理页面等待管理员决定；确认之前不丢弃数据。
- 仅允许丢弃管理员明确确认的、不兼容的插件数据，范围与任务协调规则如下。
- 数据不兼容与宿主 Interface 不兼容必须区分：允许丢弃数据不能让宿主执行其无法支持的插件契约。

## 已确认的数据丢弃范围

| 数据 | 处理规则 |
| --- | --- |
| 不兼容的供应商选项与插件私有状态 | 明确列出受影响连接及数据后重置。 |
| 不兼容的上游凭据 | 明确确认后清除，要求重新登录或填写。 |
| 不兼容的模型发现元数据 | 清除后重新发现，恢复前不得用不兼容元数据执行。 |
| Provider 的 ID、名称与兼容的连接配置 | 保留，不因数据重置删除整个连接。 |
| Route / Target 绑定 | 保留，不自动删除或改绑；受影响 Target 在必要数据恢复前暂不可用。 |
| 历史、请求记录、已确认用量与额度历史采样 | 保留，不因插件更新删除。 |

确认界面必须列明哪些连接会丢弃哪些数据及恢复动作，不使用笼统的“清空插件数据”代替影响说明。其他未受影响连接与兼容的数据不在丢弃范围内。

## 已确认的不兼容更新任务协调

- 管理员确认不兼容更新后，立即停止该 Vendor 的新任务准入，并取消该 Vendor 的全部活跃任务，不等待业务自然完成。
- 取消范围覆盖该 Vendor 的所有连接及待配置认证会话，包括推理、完整搜索、媒体生成、OAuth 登录与交换、凭据刷新、模型同步、额度查询及插件辅助调用；不仅限于需要丢弃数据的连接。其他 Vendor 不受影响。旧搜索报告与图片结果不能在取消后迟到提交。
- 确认界面同时说明数据丢弃范围和全部活跃任务将被中断的影响，不能只提示数据清理。
- 取消后必须确保旧版本任务不能继续执行或写回数据，再实施已确认的数据重置与版本切换；旧认证会话的迟到回调不得恢复已取消会话或写入凭据。等待取消清理不等于等待业务自然完成。
- 宿主取消不能保证上游已经停止执行或停止计费；保留已确认用量，不伪造未知用量，也不自动重放被中断的请求。
- 新版本启用后，具备有效配置与凭据的连接可以恢复接入；必要数据已清除的连接继续保持暂不可用，直到管理员重新配置或登录。
- 若无法确保旧任务已经停止且不能写回，不得继续丢弃数据或宣称切换成功。该取消例外不改变兼容更新允许在途调用使用旧版本完成的规则。
