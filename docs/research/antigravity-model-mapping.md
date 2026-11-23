# Antigravity 模型同步与映射调研

调研日期：2026-10-05。范围：官方 CLI 1.2.16 静态证据、Stravia 实施前的模型发现与同步路径、截图中同名不同 ID 的解释边界，以及经用户授权的官方 CLI 账号模型清单验证。第 1–7 节记录调研阶段，没有修改产品实现或向 Google 发送推理请求；用户随后批准的实现与隔离验证见第 8 节。

## 结论

Antigravity consumer CLI 从 `fetchAvailableModels` 获取账号模型目录，响应不仅包含 `models`，还包含 Agent 模型分组/排序、默认模型、用途清单与废弃模型重定向。官方 CLI 分别构造完整 ModelInfo 清单和面向 Agent 的选择配置，并有独立的模型名称/effort 解析。

Stravia 实施前只读取 `models` 的一个子集，按 map key 保存上游模型 ID，不消费 `agentModelSorts`、`defaultAgentModelId` 或 `deprecatedModelIds`，也不把两条同名记录合并。第 8 节的实现已改为官方 Agent 引用筛选与家族档位合并。

截图中的 `gemini-pro-agent` 与 `gemini-3.1-pro-high` 都显示 `Gemini 3.1 Pro (High)`，符合当前“不同 ID 可以同名”的实现。进一步核对官方 CLI 的 slug 映射后，已直接确认 `gemini-pro-agent` 的用户可见 slug 是 `gemini-3.1-pro-high`；本次官方 `models` 输出也只有一个 Pro High 选项。这证明 CLI 显示映射，不证明 Google 服务端两个请求 ID 使用相同模型快照。

## 1. 官方证据与分析方法

官方公开 [CLI 仓库固定提交](https://github.com/google-antigravity/antigravity-cli/tree/65a3c69e388148c9327f307efe82ddb1c0c8d7d4) 不提供所需的网络层实现源码；本次分析的是 [CLI 1.2.16 Linux x64 发布包](https://github.com/google-antigravity/antigravity-cli/releases/download/1.2.16/agy_cli_linux_x64.tar.gz)，不是完整源码反编译。

下载归档为 61,494,901 字节，SHA-256 与 [GitHub release API](https://api.github.com/repos/google-antigravity/antigravity-cli/releases/tags/1.2.16) 的 asset digest 一致：

```text
d4247430e04cebdbe1ca93d9ccb483cd2f3daeb4cdb0ace5a71cd130e0bdab84
```

归档中的 ELF 为 209,625,296 字节。Linux 二进制仅静态读取，没有执行；Windows 官方 CLI 的实际运行见第 5 节。解码 ELF 内嵌 `FileDescriptorProto`，恢复 Go PCL 函数名，并用 `llvm-objdump` 核对相关函数调用。`model_configs.proto` 描述符位于文件偏移 87,486,740，长度 5,955 字节；原始名称为 `google/internal/cloud/code/v1internal/model_configs.proto`。

以下结构来自该描述符，不是社区代理猜测，也不代表某个真实账号此次响应中每个字段都有值。

### FetchAvailableModelsResponse

|编号|JSON 字段|描述符类型与结构|
|---|---|---|
|1|`models`|`map<string, ModelDetails>`|
|2|`defaultAgentModelId`|`string`|
|3|`agentModelSorts`|`ModelSort[]`|
|4|`commandModelIds`|`string[]`|
|5|`tabModelIds`|`string[]`|
|6|`imageGenerationModelIds`|`string[]`|
|7|`mqueryModelIds`|`string[]`|
|8|`webSearchModelIds`|`string[]`|
|9|`deprecatedModelIds`|`map<string, DeprecatedModelReroutingInfo>`|
|10|`commitMessageModelIds`|`string[]`|
|11|`audioTranscriptionModelIds`|`string[]`|
|12|`battleModeModelSorts`|`ModelSort[]`|
|13|`experimentIds`|`int32[]`|
|14|`tieredModelIds`|`TieredModelConfig`，包含 `flashLite`、`flash`、`pro` 三组 `string[]`|

关键嵌套结构：

- `ModelSort`：`displayName`、`groups: ModelGroup[]`。
- `ModelGroup`：`displayName`、`modelIds: string[]`。分组通过 ID 引用模型目录，不通过显示名引用。
- `DeprecatedModelReroutingInfo`：`newModelId: string`、`oldModelEnum`、`newModelEnum`；后两者为 `.exa.codeium_common_pb.Model` enum。
- `ModelDetails`：包括 `displayName`、`supportsImages`、`supportsVideo`、`supportsPdf`、`supportsThinking`、`maxTokens`、`maxOutputTokens`、`recommended`、`disabled`、`isInternal`、`model` 等。`model` 是 enum，不是目录 map key。
- `FetchAvailableModelsRequest` 声明 `project`、`requestId`、`location`、`entitlement`。Stravia consumer 路径目前只发送 `project`；不能把这个子集说成官方请求完整结构。

### 官方实现的可验证调用链

下列函数名前缀分别为 `google3/third_party/jetski/language_server/code_assist_client/codeassistclient` 和 `google3/third_party/jetski/cli/backend/backend`。地址仅对应上述固定 ELF 的链接虚拟地址。

|函数|地址范围|反汇编直接观察|
|---|---|---|
|`(*CodeAssistClient).fetchAvailableModels`|`0x76a47c0–0x76a4c60`|consumer 网络分支调用 `getCAICProject` 与 `doRequestAndUnmarshal`；传入原始字符串 `fetchAvailableModels`。另有 Gemini API-key、gateway、ADC 分支，不能混为 consumer 路径。|
|`(*CodeAssistClient).GetModelInfos`|`0x76a65c0–0x76a67a0`|调用共享 `Cache.Get`，遍历 `models` map，将 key 与详情传入 `ModelDetailsToModelInfo`。|
|`(*CodeAssistClient).GetCascadeModelConfigData`|`0x76a6e40–0x76a7280`|调用同一缓存；对 Agent 与 battle sorts 分别调用 `processModelSorts`，并读取默认 ID，缺失时从首组首 ID 回退，再查 `models` 获取 enum。|
|`processModelSorts`|`0x76a67a0–0x76a6e40`|按 sorts/group IDs 查模型目录，并调用 `modelDetailsToClientModelConfig`、`modelSortToClientModelSort`，不是按相同 displayName 合并目录。|
|`(*CodeAssistClient).GetDeprecatedModelReroute`|`0x76a5680–0x76a57c0`|读取缓存的 `deprecatedModelIds` 值，按传入 enum 比较 `oldModelEnum`，匹配时返回 `newModelEnum`；无匹配返回原 enum。此函数不是简单地取 `newModelId` 发请求。|
|`(*ServerBackend).GetModelConfig`|`0x831f180–0x8320460`|先调用 `GetCascadeModelConfigData`，再调用 `GetModelInfos`；后续有 `UserFacingSlug`、`appendCustomModels`、`sort.SliceStable`。|
|`ResolveModel`|`0x8319e40–0x831a1e0`|逐个调用 resolver；标准 resolver 使用 `GetModelInfoFromNameWithContext`，还比较 label/slug/enum 的字符串表示，调用 `LogModelRewrite`。|

进一步检查 `GetModelConfig` 的数据流：`GetCascadeModelConfigData` 的结果保存在栈偏移 `0x1c8`，模型选项构造循环遍历该结果中的 configs；`GetModelInfos` 的结果保存在 `0x190`，用于辅助 enum 查找，而不是无条件把完整目录加入选项。因此仅同步 `models` 会扩大官方 Agent 选择范围。分组中的 ID 在 `processModelSorts` 内以字符串集合收集，再转换对应详情；不是按显示名或 enum 去重。

`commands.resolveModelName`（`0x86ebf00–0x86ec0c0`）调用 `ResolveEffort` 与 `ResolveModel`。官方 [CHANGELOG 1.1.28](https://github.com/google-antigravity/antigravity-cli/blob/65a3c69e388148c9327f307efe82ddb1c0c8d7d4/CHANGELOG.md#1128) 也明确记录 alias resolution、`--effort` variant selection 和 deprecated saved model replacement 的日志行为。

因此需区分三种映射：云端目录 ID 到模型详情、Agent 选择分组/默认项、用户名称/effort/废弃模型的执行解析。它们不是同一个 map。

本次没有验证缓存 TTL、全部 resolver 的优先级或全部身份分支；不能从符号名称推断这些细节。

## 2. Stravia 实施前的同步流程

```text
POST /providers/{id}/models/sync
  → AdminService::sync_provider_models
  → RouteModule::sync
  → 插件 Discover
  → POST /v1internal:fetchAvailableModels，body={"project":"该账号 project"}
  → 解析 models map
  → 补充目录规格
  → 按 provider_id + model_id 原子协调数据库记录
```

源文件证据：

- HTTP 入口：[providers.rs](../../backend/apps/stravia-server/src/admin_routes/providers.rs)，287–294 行。
- 插件发现：[catalog.rs](../../backend/crates/stravia-vendor-antigravity/src/catalog.rs)，9–106 行。
- project 与 HTTP 地址：[client.rs](../../backend/crates/stravia-vendor-antigravity/src/client.rs)，24–53、106–175 行。project 来自连接凭据或绑定会话，不凭模型名猜测。
- 宿主协调：[provider_model_records.rs](../../backend/crates/stravia-core/src/admin/routes/provider_model_records.rs)，474–603 行。

### 字段投影

|界面/存储字段|当前来源|
|---|---|
|模型 ID|`models` 的 map key；不采用 `ModelDetails.model` enum|
|名称|`displayName`，trim 后为空或缺失时回退 ID|
|上下文|正整数 `maxTokens → limit.context`|
|输入模态|始终 `text`；对应 `supportsImages/Video/Pdf=true` 时加入 image/video/pdf|
|输出模态|当前插件固定 `text`，不是从上游输出类型完整推导|
|Thinking 信息|保存 `supportsThinking` 与提供的 budget/level；不从 budget 编造离散 effort|
|过滤|跳过 `disabled=true` 或 `isInternal=true`|

插件按所选 variant 保留 `maxOutputTokens` 用于无显式输出上限时的推理默认值；仍不消费上述默认项、分组、用途清单和废弃重定向之外的所有详情字段，例如 `recommended`。尤其截图中的 Image 命名与文本输出图标并不矛盾：这是当前投影限制，不能证明该模型实际不支持图像生成。

宿主元数据模板优先级为 Provider Catalog scope、Canonical fallback、bare discovery；之后 discovery 同名字段覆盖模板，并强制保存原始发现 ID。Canonical 只补规格，不改写上游调用 ID。依据：[provider_model_records.rs](../../backend/crates/stravia-core/src/admin/routes/provider_model_records.rs)，619–687、736–803 行；[provider_catalog/mod.rs](../../backend/crates/stravia-core/src/provider_catalog/mod.rs)，678–709 行。

未被用户接管的 Imported 记录跟随来源刷新；Edited 保留规格，仍刷新插件托管字段；Manual 记录跳过。新增/缺失/恢复按 ID 协调，不按名称去重，缺失记录保留而不是删除。实现依据为上述 `RouteModule::sync`，不能只依据较旧 [ADR-0054](../adr/0054-persist-provider-model-snapshots.md) 的“Imported 保持不变”表述；该处文档与当前实现存在偏差，本次不修改其决策内容。

推理代码直接将 Target 的 `provider.model` 放入 envelope `model`，没有将 `gemini-pro-agent` 改为 `gemini-3.1-pro-high`，也没有反向改写：[wire.rs](../../backend/crates/stravia-vendor-antigravity/src/wire.rs)，34–42、149–159 行。

## 3. 截图同名模型与建议

截图只能证明界面有两条同名不同 ID 的记录。独立运行当前解析器的合成输入证明，即使两条记录具有相同 `displayName` 和相同 `model` enum，它们仍保持两个 ID，`selector=None`。这不是截图账号的原始响应验证。

官方 [Models 文档](https://antigravity.google/docs/models.md) 展示 Gemini 3.1 Pro 的 High/Low 选项，但不公布上述两 ID 之间的别名关系。新增的直接证据来自 CLI 二进制，而不是该页面或第三方代理：`backend.map.init.0`（`0x8316300–0x8316540`）将 `gemini-pro-agent` 登记为 `{base: "gemini-3.1-pro", effort: "high"}`；`slugFor`（`0x831bdc0–0x831be60`）先查该映射，`UserFacingSlug`（`0x831c000–0x831c080`）连接 base 与 effort，得到 `gemini-3.1-pro-high`。

若目标是对齐官方 Agent 选择器，应从响应的 `agentModelSorts[].groups[].modelIds` 构建选择分组，结合 `defaultAgentModelId`、真实 `deprecatedModelIds` 与官方 user-facing slug。实际请求 ID 和显示 slug 必须分开保留，不能因为 slug 映射就擅自替换上游请求 ID；也不能按同名或同 enum 合并。完整账号模型目录可以另行保留。用途清单和 Agent 清单应区分。是否改变 Stravia 的展示/选择/执行契约属于后续产品决策，本次没有实施。

## 4. 实际验证

1. `task build:vendors:all`：成功生成八个自包含 Wasm 组件。
2. `cargo test --locked -p stravia-vendor-antigravity --test contract oauth_and_inference_enforce_native_wire_at_real_http_boundary -- --exact --ignored --nocapture`：1 passed；实际加载 Wasm，经过 loopback HTTP 发现账号 ID、过滤 internal，并验证 OAuth、项目 operation、推理协议边界等。没有连接 Google。
3. 临时 `cargo run --locked -p stravia-vendor-antigravity --example model-mapping-smoke`：包含原始 catalog/client/auth 模块，给解析器传入合成双 ID 同名目录；输出保留 `gemini-3.1-pro-high` 与 `gemini-pro-agent` 两条，context 均为 1048576，模态均为 text/image/video 输入、text 输出，selector 均为 None。临时入口首次缺失 client 模块而编译失败；补齐原模块后运行成功，之后已删除入口，没有增加永久测试。
4. 官方 Linux 归档哈希校验、内嵌 protobuf 字段解码与上述函数的静态反汇编完成。Linux CLI 未执行，临时二进制分析文件已清理，不随仓库保存。

## 5. 官方 Windows CLI 的账号清单验证

用户明确授权官方 CLI 登录，由用户完成 Google 授权并提交授权码。使用 [CLI 1.2.16 Windows x64 发布包](https://github.com/google-antigravity/antigravity-cli/releases/download/1.2.16/agy_cli_windows_x64.zip)，归档大小为 57,992,904 字节，SHA-256 与 release API 的 asset digest 一致：

```text
07882fb316a19a9cad68265fe9bb1c926f0b5084267d3dc198c0960189f77936
```

临时目录中运行原始 `antigravity.exe --help`、`antigravity.exe models --help`，确认存在专用只读 `models` 子命令。第一次 `models` 因未登录返回 `Please sign in to view available models`；官方 OAuth 登录后再次运行成功，退出码 0，耗时 5.33 秒，输出按原顺序为：

```text
gemini-3.8-flash-high       Gemini 3.8 Flash (High)
gemini-3.8-flash-medium     Gemini 3.8 Flash (Medium)
gemini-3.8-flash-low        Gemini 3.8 Flash (Low)
gemini-3.7-flash-high       Gemini 3.7 Flash (High)
gemini-3.7-flash-medium     Gemini 3.7 Flash (Medium)
gemini-3.7-flash-low        Gemini 3.7 Flash (Low)
gemini-3.6-flash-high       Gemini 3.6 Flash (High)
gemini-3.6-flash-medium     Gemini 3.6 Flash (Medium)
gemini-3.6-flash-low        Gemini 3.6 Flash (Low)
gemini-3.1-pro-high         Gemini 3.1 Pro (High)
gemini-3.1-pro-low          Gemini 3.1 Pro (Low)
claude-sonnet-4-6          Claude Sonnet 4.6 (Thinking)
claude-opus-4-6-thinking    Claude Opus 4.6 (Thinking)
gpt-oss-120b-medium        GPT-OSS 120B (Medium)
```

共 14 个模型选项。清单中没有独立的 `gemini-pro-agent`，也没有截图可见的 Gemini 2.5 Pro、Gemini 3 Flash、Gemini 3.1 Flash Image、Gemini 3.1 Flash Lite 或 Gemini 3.5 Flash High。不能据此声称它们从完整上游目录删除或已不可调用：验证的是官方 CLI 当前账号的 Agent 选择清单，不是完整目录，更不是逐模型推理测试。

没有运行 `-p`、提交普通提示词、选择 `/model` 或修改模型配置。认证凭据由官方 CLI 自己保存，不记录在研究文件中。交互 onboarding 随后出现 Terms of Service & Data Use 页面，未提交其 Done 按钮，也未额外同意交互数据收集；该交互进程已终止。临时 Windows/Linux 二进制已清理，官方 CLI 保存的登录凭据按已授权范围保留。

限制：没有记录截图实例的原始响应，也没有证明本次登录账号与截图账号及发现时点完全相同；未验证全部模型可调用性、所有身份分支或完整内部行为。第 6 节补充了同一已登录官方 CLI 的实际响应字段验证，但不能追溯截图实例。私有接口与映射值随版本、账号和计划变化；描述符存在某字段不代表它当次一定返回。

## 6. 官方 Agent 清单的实际筛选依据

继续使用上述已校验的官方 Windows CLI 1.2.16，不读取或导出登录凭据。在 LLDB 中运行只读 `models` 子命令，于实际 protobuf JSON 解码入口暂停，仅在内存中解析响应并提取模型字段。成功捕获的 `FetchAvailableModelsResponse` 为 205,577 字节，`models` 包含 33 个目录条目，`agentModelSorts` 则只引用 14 个 ID。原始响应、项目标识、账号计划和实验值不保存到研究文件。

当次唯一的 sort 为 `Recommended`，其 group 按顺序引用：

```text
gemini-3.8-flash-high
gemini-3.8-flash-medium
gemini-3.8-flash-low
gemini-3.7-flash-high
gemini-3.7-flash-medium
gemini-3.7-flash-low
gemini-3.6-flash-high
gemini-3.6-flash-medium
gemini-3.6-flash-low
gemini-pro-agent
gemini-3.1-pro-low
claude-sonnet-4-6
claude-opus-4-6-thinking
gpt-oss-120b-medium
```

`defaultAgentModelId` 为 `gemini-3.8-flash-high`。它决定默认项，不会把未引用的目录记录补入 Agent 清单。

### 截图中五个缺席项的直接证据

以下五项在本次 `models` 中均存在，按 protobuf 默认值解释后的 `disabled=false`、`isInternal=false`，但均未出现在 `agentModelSorts[].groups[].modelIds`。因此当次未列入 Agent 选择器的原因是用途清单未引用，不是禁用、internal 或目录删除。

|名称|上游目录 ID|其他用途清单的引用|
|---|---|---|
|Gemini 2.5 Pro|`gemini-2.5-pro`|本次没有用途清单引用|
|Gemini 3 Flash|`gemini-3-flash`|`commandModelIds`|
|Gemini 3.1 Flash Image|`gemini-3.1-flash-image`|`imageGenerationModelIds`|
|Gemini 3.1 Flash Lite|`gemini-3.1-flash-lite`|`webSearchModelIds`|
|Gemini 3.5 Flash (High)|`gemini-3-flash-agent`|本次没有用途清单引用|

目录 ID 不一定与显示版本一致：例如当次 `gemini-3-flash-agent` 显示为 Gemini 3.5 Flash (High)。不能根据 ID 前缀或显示名称推断用途、禁用状态或实际后端版本。

### 筛选字段各自的职责

1. `agentModelSorts[].groups[].modelIds` 是官方 Agent 选项的引用清单和顺序来源；`models` 只是详情目录。`GetCascadeModelConfigData → processModelSorts` 按引用生成 config，而不是将整个目录全部加入。
2. `models[id].disabled` 投影为 `ClientModelConfig.disabled`，`ServerBackend.GetModelConfig` 跳过禁用 config。静态字段链验证了 `ModelDetails.GetDisabled` 读取偏移 `0x59`，转换写入 config 偏移 `0x28`，最终选择器检查该偏移。
3. `isInternal` 被投影到另一条 `ModelInfo` 路径；不能将 Stravia 当前的 `disabled || isInternal` 过滤等同于官方 Agent 引用筛选。上述五个缺席项的两标记都为 false，足以直接排除这两个原因。
4. `deprecatedModelIds` 提供旧 ID/enum 的重定向，不是按名称维护的隐藏黑名单。
5. `commandModelIds`、`imageGenerationModelIds`、`webSearchModelIds` 等属于其他用途；被它们引用不等于可在 Agent 选择器中选择。`tieredModelIds`、`recommended`、能力字段也不能替代 Agent 引用清单。

### Pro 同名项的关系已由实际响应确认

本次响应包含：

```json
{
  "gemini-3.1-pro-high": {
    "newModelId": "gemini-pro-agent",
    "oldModelEnum": "MODEL_PLACEHOLDER_M37",
    "newModelEnum": "MODEL_PLACEHOLDER_M16"
  }
}
```

这是 `deprecatedModelIds` 的真实条目，不是合成测试数据。两条目录记录的 enum 不同；Agent 清单只引用 `gemini-pro-agent`，官方显示 slug 再映射为 `gemini-3.1-pro-high`。因此可以依据真实重定向与官方别名收敛为一个 High 选项，不能依据同名或相同 enum 去重，更不能由此声称两个后端快照相同。

### 交叉验证与边界

从当次原始响应提取 Agent 引用、排除 disabled、应用已证实的 Pro 显示别名后，所得 14 个 slug 及顺序与再次运行原始 `antigravity.exe models` 的标准输出完全一致，退出码 0。额外断言验证上述五项存在于目录、两标记为 false、均不在 Agent 引用清单中。调试进程退出码 0，临时官方二进制随后清理。

此验证没有提交推理提示词、修改模型配置或重新登录。它证明本次账号响应的筛选原因，不证明未列入 Agent 清单的模型可调用，也不解释服务端为什么为该账号选择这些引用。没有永久测试或产品实现变更。

## 7. 按家族合并展示的目标

若按本次官方 Agent 清单展示，14 个选项可以整合为 7 行：

|模型家族|同一行显示的档位或能力标签|
|---|---|
|Gemini 3.8 Flash|High / Medium / Low|
|Gemini 3.7 Flash|High / Medium / Low|
|Gemini 3.6 Flash|High / Medium / Low|
|Gemini 3.1 Pro|High / Low|
|Claude Sonnet 4.6|Thinking|
|Claude Opus 4.6|Thinking|
|GPT-OSS 120B|Medium|

目标是不删除存量逐档记录或丢失请求身份。每个选项必须保留实际请求 ID、官方显示 slug 与自身能力/限制；选择 Pro High 时应保留 `gemini-pro-agent`，Pro Low 则保留 `gemini-3.1-pro-low`。不从显示名称自动生成额外档位，Thinking 标签也不自动转换为 High/Medium/Low。家族发现记录与每档规格的实际保存方式见第 8 节。

推荐先按官方 Agent 引用筛选，再按官方 base/effort 与真实重定向分组。完整上游目录与各用途仍需区分；不要用版本黑名单隐藏模型，或以同名去重替代上游契约。本次仅明确整合展示规则，没有修改 Stravia 界面、同步或推理行为。

## 8. 用户批准后的实现与隔离验证

实现位于 [catalog.rs](../../backend/crates/stravia-vendor-antigravity/src/catalog.rs)、[selector.rs](../../backend/crates/stravia-vendor-antigravity/src/selector.rs)、[wire.rs](../../backend/crates/stravia-vendor-antigravity/src/wire.rs)，沿用 Devin 的家族、默认 selector 和插件专有选择表模式，但不复用 Devin 的模型解析规则或请求协议。发现按 Agent 引用筛选、解析真实重定向，并将官方 slug 中明确登记的 effort 收敛为家族记录；不按显示名或 enum 合并。

家族记录用现有 `DiscoveredModel.family`、`selector`、`metadata.reasoning_efforts` 表达。`metadata.antigravity` 保存默认请求 ID 与 `variants`，每档保留实际 ID、显示 slug、effort 和规格。家族上下文采用所有已知档位的最小值，任一档未知则不登记；输入模态取交集。Thinking 保留在名称中，不伪造 Effort。没有新增公共 API、数据库 schema、前端供应商分支或依赖。

推理根据 Target 的 Effort 从选择表取真实 ID 和该 variant metadata 的强度控制：优先使用存在且合法的 `thinkingLevel`，只有未声明 level 时才读取 `thinkingBudget`，不同时发送两者。缺省使用该家族的默认 selector 与目录强度，显式 Target 非 Effort 控制或合法原始强度字段优先于目录缺省强度。未登记 Effort、对应的两种控制均缺失或所选控制无效时，在 HTTP 前返回明确错误；非法 level 不回退到 budget，也不从 `minThinkingBudget` 猜预算。已消费的 Effort 不再让通用 codec 生成额外强度字段，摘要开关独立。已有逐档记录与 Route 绑定不自动删除或迁移，部署步骤见 [插件设计说明](../design/vendor-plugins.md#antigravity--antigravity)。

CLI 1.3.2 曾记录 Gemini 3.8 Flash Medium/High 的目录预算 `4000`/`-1`，但当前目录已可仅声明 `thinkingLevel`。2026-10-28 的真实 Wasm + loopback HTTP 回归以 Flash 的 level-only 三档和 Pro 的 budget 目录（缺省/Low/High 请求）覆盖两种目录形态，并校验默认 ID、显式控制优先级、独立摘要、level/budget 互斥及非法元数据在 HTTP 前拒绝；未调用真实上游。

### 2026-11-13：CLI 1.5.8 Hydrogen 目录与推理

经授权的 consumer 账号使用 Linux/amd64 官方 [agy 1.5.8](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.8)，经本机捕获桥访问真实 daily 端点。发布包按官方 manifest 的 SHA-512 核验后执行。私有采集覆盖真实目录与 Low/Medium/High/XHigh 请求，脱敏同连接证据保存在 [inference fixture](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.json)及 [XHigh fixture](../../backend/crates/stravia-core/src/plugin/network/shape/fixtures/antigravity-inference.repeat.json)；不提交凭据、账号响应或私有原文。该次目录四档均声明 `maxTokens=1048576`、`maxOutputTokens=262144`，Low/Medium/High/XHigh 的 `thinkingLevel` 分别是 1/2/3/100。实际推理请求分别写 `LOW`、`MEDIUM`、`HIGH`、数字 `100`，均请求 `includeThoughts=true`。XHigh 的 100 不是 master enum 的 7，不能按 effort 名猜测转换。账号目录和调用仅证明该时点该账号行为，不保证其他账号或未来版本一致。

目录所选 variant 的输出上限作为无显式下游上限时的默认值；客户端显式值（包括 0）优先，目录缺失时保留旧默认 65536。上线需部署更新后的 vendor Wasm 并重新同步模型，使保存的 selector 元数据包含每档输出上限和新 enum 值；仅更换插件不会自动更新已有目录快照。

原生 `agy --model gemini-4-hydrogen --print "Reply OK"` 在全新 HOME、未指定 effort 时实际选择 Medium 并返回 200，即使 `agentModelSorts` 的 Hydrogen 顺序为 High、XHigh、Medium、Low。1.5.8 二进制 `backend.DefaultEffortForBase` 的分支确认：目录默认型号属于当前家族时保留其档位，否则优先已有 Medium，再回退首档；不是把展示排序首项一律当默认，也不是按 Hydrogen 名称特判。选择器发现阶段同步采用此优先级，客户端显式 Effort 仍独立覆盖。默认选择回归先失败后通过；最终 Gateway 省略 reasoning 的真实请求同样选择 Medium，发送 262144 上限、MEDIUM 与 includeThoughts=true，并返回 HTTP 200、`OK`。

本轮隔离 Gateway 实测从全新 SQLite 和实际 Wasm 开始，经 `/v1/responses`、宿主 TLS 与捕获桥访问真实 Google；Target retry budget 为 0，不修改生产数据或复制刷新令牌。修复前的 Wasm 在前三档返回 200，但输出上限为 65536；XHigh 在发出推理前返回 400。重建后四档均返回 HTTP 200 和 `OK`，模型 ID、完整 generationConfig、请求头名称顺序与非动态值均匹配原生捕获。ClientHello 的 JA3/JA4、算法和扩展顺序匹配；扩展细节唯一差异是 SNI 长度：原生覆盖端点为 localhost，Gateway 保留 Google hostname。未复制 CLI 的 system prompt、内置工具或操作树 labels，也不声称完整正文逐字一致。

本轮首轮执行两个插件的库测试，119 项通过；补充默认选择和新型号模态回归后分别重跑，Antigravity 29 项、Claude 92 项通过。Antigravity 实际 Wasm/HTTP contract 5 项、Claude contract 19 项、Core shape 11 项及两个 Gateway TLS/header 验收用例通过，共 158 项非 live 用例。`cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors -- --all` 重建八个组件，两个插件的 all-target Clippy（`-D warnings`）、`cargo check --locked -p stravia-core` 及格式检查通过。真实调用由一次性 Gateway smoke 入口完成，不将本地合成 contract 等同于上游证明。临时入口、捕获服务、凭据副本与原始私有流量在验证后删除；仓库仅保留脱敏原生 fixture。未运行完整仓库、UI 或 Tauri 测试矩阵。

以下为本节早期家族合并实现的历史验证，不代表 1.5.8 新断言已执行：

- `cargo test --locked -p stravia-vendor-antigravity --lib`：15 passed。覆盖 Agent 引用筛选、其他用途排除、默认档位、重复引用、同名身份分离、真实重定向与循环拒绝、规格交集及未登记 Effort 拒绝。
- `task build:vendors:all`：增量重建 Antigravity，生成八个自包含组件。
- `cargo test --locked -p stravia-vendor-antigravity --test contract oauth_and_inference_enforce_native_wire_at_real_http_boundary -- --exact --ignored --nocapture`：历史验证 1 passed。当时实际 Wasm 与 loopback HTTP 验证默认/Low/Medium/High 的 envelope ID，以及未登记 Effort 不发送 HTTP；原有 OAuth、签名、SSE 和错误边界通过。后续 CLI 1.3.2 对齐已将回归断言改为同时保护 ID、variant 预算、独立摘要和显式控制优先级；此处不把历史结果视为新断言的执行结果。
- `cargo clippy --locked -p stravia-vendor-antigravity --all-targets -- -D warnings`：通过。
- 在全新临时 SQLite 实例导入实际构建的插件，重放第 6 节响应中已去除账号信息的模型字段。OAuth 测试状态直接在隔离存储中预置假 token 与假 project；没有重新登录或读取真实账号凭据。真实同步返回 `added=7`，模型 API 输出七个家族。浏览器实际点击添加 Pro 家族，保存的 Route 仅支持 `low`、`high`；Flash 家族支持 `low`、`medium`、`high`。
- 经真实 Server 的 `/v1/chat/completions` 向 loopback 上游发送请求：Pro 缺省与 High 为 `gemini-pro-agent`，Low 为 `gemini-3.1-pro-low`，Flash Medium 为 `gemini-3.8-flash-medium`；校验上游捕获的 envelope 与客户端响应一致。
- Chromium 实际页面核对英文深色桌面表格、中文浅色 390px 移动列表，Pro 只有一行并显示 `low, high`，Flash 同行显示三档；未显示 Gemini 2.5 Pro 等非 Agent 项，浏览器无错误。复用现有 Svelte 页面，没有修改前端源码。

临时服务、存储、模型响应夹具与截图在验证后清理。上述请求只访问本地假上游，不证明真实 Google 推理可用性；未运行完整仓库测试矩阵或 Tauri 原生宿主。本轮首次编译的错误枚举名称已修正为项目既有 `ErrorKind::Invalid`，后续列出的检查均为修正后结果。

### 2026-11-18：Sonnet 5.5 Medium 与 Target 禁用的区别

`agy` 1.5.11 的真实 Sonnet 5.5 Medium 交互请求使用 `claude-sonnet-5-5-medium`、`maxOutputTokens: 128000` 和 `thinkingConfig: {"thinkingLevel":"MEDIUM","includeThoughts":true}`，没有 `thinkingBudget`，上游返回 HTTP 200。当时运行的 Stravia 没有指向 Antigravity Sonnet 的推理路由；[契约测试](../../backend/crates/stravia-vendor-antigravity/tests/contract.rs)在真实 Wasm 到 loopback HTTP 的边界检验同一组字段，不将它当作线上 Sonnet 抓包。

诊断时先核对同一次调用的 `effective_request.reasoning.target_control`，不能从出站模型名的 `-medium` 后缀反推强度。一条 Hydrogen 历史请求的入站为 `effort: high, summary: auto`，但实际生效的 Target 控制是 `disabled`。插件保留默认 Medium 模型 ID，按显式控制发送 `thinkingBudget: 0, includeThoughts: false`，而不是按目录发送 `thinkingLevel`。若期望 Medium 强度，应将对应 Target 档位映射到 `Effort(medium)`，而非 `Disabled`；无需更改全局摘要默认值。这条历史记录不能解释另一次请求的 429。

## 9. 官方有 3.7/3.8 而 Stravia 缺项的根因

用户升级后的桌面截图只有五个家族，而同一已登录官方 CLI 仍列出 Gemini 3.7／3.8 Flash。只读检查桌面 SQLite：五条家族记录为 `present`，共有八个档位；另有 23 条旧记录为 `missing`，包括 `gemini-3.7-flash-tiered` 和 `gemini-3.8-flash-tiered`。页面筛选解释了旧记录隐藏，但不能解释官方与插件为何拿到不同 Agent 选项。

使用该连接已保存且仍有效的 token 与 project，只读调用 `fetchAvailableModels`。在 LLDB 中捕获官方 CLI 发送前的实际 HTTP 请求，仅输出公开客户端标识、端点、请求字段与 project 相等性，不输出凭据或项目值。官方使用相同 daily 端点，请求体也只有相同的 `project`；其 User-Agent 实际为：

```text
antigravity/cli/1.2.17 (aidev_client; os_type=windows; arch=amd64; cl=993434119; auth_method=consumer)
```

该二进制来自已校验的官方 1.2.16 发布包；发布标签与请求内构建版本不相同，不能只拼接发布标签推断完整客户端标识。

固定同一 token、project、端点和请求体，逐项差分得到：

|只改动的条件|目录项目数|Agent 档位数|Gemini 3.7／3.8 Agent 档位|
|---|---:|---:|---|
|原插件 `antigravity/1.2.16 (...)`|27|8|未返回；只有不被 Agent 引用的 tiered 目录项|
|仅把 `os_type=linux` 改为 `windows`|27|8|未返回|
|仅切换为 production Cloud Code 域名|27|8|未返回|
|替换为捕获的完整官方 User-Agent|33|14|均返回|
|原插件标识仅补回 `/cli/`，版本仍为 1.2.16，Linux/amd64，不带构建号|33|14|均返回|
|带 `/cli/`，版本改为 1.2.17，Linux/amd64，不带构建号|33|14|均返回|

根因是原 `CLI_USER_AGENT` 漏掉 `/cli/`，导致实际上游返回不同模型集合，不是家族合并删除模型，也不是同账号差分中的账号资格、project 或平台差异。最小修复只补回该片段，不加入额外头、请求字段、端点回退或权限变化。此证据说明当前响应行为，不声称掌握 Google 内部分类规则或保证所有账号返回相同模型。

回归在既有真实 Wasm/HTTP 契约中模拟已观察到的客户端分类：非 CLI 请求不提供新 Flash 的 Agent 档位，断言保护完整家族发现，而不绑定版本、平台或构建号的完整字符串。修复前该断言实际失败，缺少 `gemini-3.8-flash`；重建独立插件后同一用例通过。`task build:vendors:all`、15 个插件单测、真实 Wasm 契约及定向 Clippy 均通过。

另启动全新临时 Server，导入本轮实际构建的 Wasm，仅在隔离存储中预置当前有效 token 与 project，不重新授权、刷新 token 或复制账号其他资料。通过真实管理同步接口访问 Google：HTTP 200，`added=7`，模型列表包含 `gemini-3.7-flash` 与 `gemini-3.8-flash`。没有发送推理请求，也没有修改用户现有实例。验证后的临时 Server、凭据副本、官方调试进程与下载包均清理；使用修复仍需向实际实例导入新组件并同步模型。

## 10. Opus High：上游 429 与原生 CLI 对照（2026-11-18）

下表对照运行实例导出的 Google wire 记录和相隔约 8 分 20 秒的官方 Linux CLI 真实请求。CLI 版本 `1.5.11` 与当时[官方 Linux amd64 更新清单](https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_amd64.json)一致。CLI 在空目录发送一条短提示，捕获仅保留 HTTP 摘要；没有重放用户请求，也没有比较凭据值或 TLS 指纹。

|观测项|Stravia|官方 `agy`|
|---|---|---|
|Google 返回（UTC）|15:05:56，HTTP 429 `RESOURCE_EXHAUSTED`|15:14:16，HTTP 200，CLI 输出 `OK`|
|项目、端点、档位|与 CLI 相同|同一 `project`、daily `streamGenerateContent?alt=sse`、`claude-opus-5-5-high`|
|生成参数|`HIGH`、`includeThoughts: true`、`maxOutputTokens: 16384`|相同 thinking，`maxOutputTokens: 128000`|
|请求头与正文|头名、顺序相同；UA 声明 1.5.8；无 `request.labels`，一个工具组声明 12 项|UA 声明 1.5.11；有 `request.labels`，14 个工具组各声明一项|

入站 Responses 请求显式指定 `max_output_tokens: 16384`；不是插件误读目录默认值。运行实例保存的 High 档上限与 CLI 当次目录一致，均为 `128000`。CLI 目录列出该模型，`quotaInfo.remainingFraction: 1`。事故前后两次额度采样中，`3p-5h` 均剩 100%，`3p-weekly` 均剩约 98.98%。在事故前后各一小时的现存记录内，同一 provider 的 Opus 4 次、Gemini 6 次尝试均为上游 429；可见额度不代表所有上游限制。

查询时目标 provider 为 `use_proxy=false`（direct，不继承服务环境代理）；15:14 的 CLI 捕获走 shell 原有的 HTTPS 代理。另做一次同版本、同短提示对照：仅将 daily 端点经本地观测器直连 Google，不经过该 HTTPS 代理。15:19:55 UTC，同一 `project`、Opus High、thinking 和 128000 上限再次返回 HTTP 200，CLI 输出 `OK`。因此原生成功不依赖该代理；未测量公网 IP，也没有事故时的出口快照。两次 CLI 捕获均有本地 TLS 观测器，不能证明 Stravia 与 CLI 的上游 TLS 特征相同。

Google 错误体只有 `code`、`message` 和 `status`，没有限额名称、重试时间或 `Retry-After` 响应头。后续客户端收到 200 是另一 provider 的 fallback 成功，不是 Google 重试成功。对照证明同一项目和模型可以调用，不能把事故归因于某个请求字段；两次的时间、输入、工具和执行路径并未固定。上游响应保留了 `x-cloudaicompanion-trace-id`，可用事故 UTC 时间和该标识向服务方核查具体限制。

## 11. agy 1.5.11：MCP 工具分组、labels 和历史提示词对照

按[官方 MCP 配置](https://antigravity.google/docs/mcp.md)在临时 workspace 启动两个 stdio server：A 提供两个 eager 工具和一个默认 lazy 工具，B 提供一个 eager 工具。两个 server 都实际收到 `tools/list`。在官方 CLI 为 Opus High 发出的第一条推理请求上，于本地捕获 `request.tools` 后截断，不将该请求转发给 Google。此实验验证客户端构造，不验证上游接受或拒绝。

- 20 个 `tools[]` 项均只含一个函数声明。A 的两个 eager 工具占两个不同项，B 的 eager 工具占第三项；不是一个 server 一组。A 的 lazy 工具没有独立函数声明，另有 `call_mcp_tool`。Stravia 对齐已声明函数的分组，不负责把调用方的工具改成 CLI 的 lazy 加载模式。
- `request.labels` 是**推理正文内的字符串键值表**，不是 MCP 分组字段或 HTTP 头。本次有 9 个键，涉及操作树和轨迹（`cascade_id`、`root_cascade_id`、`trajectory_id`）、请求和步骤（`request_id`、`last_step_index`）、模型标记（`model_enum`、`used_claude`、`used_claude_conservative`、`used_non_gemini_model`）。分类来自字段名及观测类型，不代表已知服务端用途。Google 对[公开 Vertex API 标签](https://cloud.google.com/vertex-ai/generative-ai/docs/multimodal/add-labels-to-api-calls)另有说明，不能据此外推 Antigravity 私有接口的鉴权、计费或限流行为。
- 仓库过去记录过特定 OMP 系统提示片段与 429 的复现，但在本次附件可见的入站 `instructions` 和 Google 出站 `systemInstruction` 中，该旧案例的精确文本命中均为 0。本地同 provider、同模型现存 19 次 429，只有 5 次保留了可比较的上游请求：均含 OMP 身份文本，其中 4 次留存的系统指令与事故相同。该组合唯一一次成功没有留存请求正文；换 provider 后有 48 个同模型成功样本也含 OMP 身份文本。提示词、provider、时间等变量未固定，不能判定本次是提示词所致。
- [OMP #18706](https://github.com/can1357/oh-my-pi/issues/18706) 与 [CLIProxyAPI #5751](https://github.com/router-for-me/CLIProxyAPI/issues/5751) 有类似现象的用户报告，不是 Google 对原因的确认。工具分组是有抓包和 Wasm/loopback HTTP 契约支持的对齐项，不是经过验证的 429 修复。

### 原生 CLI 与插件实测的边界（2026-11-19）

- **官方 CLI 出站观察**：从 `~/.local/bin/agy` 启动的 1.5.11 会话，推理 HTTP 头自报 Antigravity CLI 1.5.11，不含 Stravia 或兼容性字样。这次在本地截断，不以此声称获得上游 200。另一次同版本原生观察中，`labels.model_enum` 与同次目录中所选 wire model 的 `models[id].model` 都是字符串且相等；`labels.request_id` 与外层 `requestId` 都是字符串但不完全相等。未推断官方 ID 的生成算法。
- **插件真实调用**：隔离加载实验 Wasm，用 Stravia 存量连接读取真实目录并发送一次普通 Opus High 推理。`model_enum` 与该档原始目录值相等，HTTP 200，返回 `OK`。实验版本使用了之后撤回的兼容 UA，使用独立宿主，不是官方 CLI 进程，也不是最终组件的完整线上验证。没有重放故障内容或写入生产库。
- **上一轮最终插件**保留每次推理的关联号、按所选档位取目录枚举、429 诊断关联，不变更既有 UA、重试策略或文本处理。没有等价 CLI 轨迹时不填充对应标签。[#5751](https://github.com/router-for-me/CLIProxyAPI/issues/5751) 是用户报告，没有证明标签或版本导致 429；其关联[诊断 PR](https://github.com/router-for-me/CLIProxyAPI/pull/5790) 本次核查时未合并。

### OMP 请求验收与九项标签来源

对照其他插件不能直接搬用字段：Codex 使用宿主的 conversation、logical turn 和 context window；Claude Code 使用宿主亲和键与有状态会话；Kiro 使用 conversation 和 user prompt。这些是本地执行事实，不证明它们等同于 Antigravity 的轨迹和步骤。相关实现位于 `model_turn/live.rs`、`stravia-vendor-codex/src/codex.rs`、`stravia-vendor-claudecode/src/lib.rs` 和 `stravia-vendor-kiro/src/lib.rs`。

|原生请求的标签|当前插件来源及覆盖边界|
|---|---|
|`request_id`|插件本次尝试的真实关联号，已发送|
|`model_enum`|所选档位同步目录中的 `model`；仅已有该字段时发送|
|`cascade_id`、`root_cascade_id`、`trajectory_id`|宿主会话与谱系不等于原生 CLI 轨迹，缺少等价生命周期依据|
|`last_step_index`|宿主 turn 序号不等于 CLI step，工具执行、重试、压缩的计数口径未验证|
|`used_claude`、`used_claude_conservative`、`used_non_gemini_model`|没有可证实的原生累计使用状态及 conservative 定义，不按当前模型名猜值|

[OMP 的公开实现](https://github.com/can1357/oh-my-pi/blob/f0200abf/packages/ai/src/providers/google-gemini-cli.ts)维护其自身的会话和步骤状态；[OmniRoute 的身份实现](https://github.com/cheaper-inference/OmniRoute/blob/bf3d1d941633a563351d16f9b8bf315a7e697610/open-sse/services/antigravityIdentity.ts)只构造部分标签。二者做法不同，均不是 Google 对字段语义或 429 原因的协议保证。本轮没有增加伪造轨迹、使用标记或更新客户端身份以规避上游限制。

#### 实际 OMP 验收结果

本机 `omp 18.22.1` 在临时 agent 目录和空 workspace 中执行非交互普通提示。未传 `--system-prompt`、`--tools` 或 `--no-tools`；保留真实 OMP 的 coding system prompt 与默认工具定义，不加载用户扩展、skills、rules 或项目上下文。OMP 经 loopback `/v1/responses`，使用仓库真实协议解码器、新 Antigravity Wasm 和 Stravia 存量连接的只读凭据，向 daily 上游发送一次推理。测试宿主禁用 HTTP 重试及重定向，不 fallback，不部署；生产数据库不写入。

实测出站为 `claude-opus-5-5-high`：系统指令 10,072 字节、11 个函数工具，包含 OMP 身份文本；`labels` 实际仅有 `request_id`，因为测试使用存量 selector 元数据，没有重新同步模型。Google 返回 HTTP **429**，OMP 退出码 **1**，没有 `OK`，工具执行事件为 **0**。因此当前插件对该 OMP 请求仍未修复 429，不以先前短提示成功代替这一验收。

验证边界：测试桥使用真正的插件与协议转换，但不是运行实例的完整 Gateway 路由/网络形态；宿主缓冲上游，再由真实编码器向 OMP 输出 SSE。这次没有加载原事故的用户规则、项目上下文或历史，不能把两次系统指令视为逐字相同。未再尝试修改提示词、标签或客户端身份重放失败请求。临时输入管道、测试入口与 OMP profile 已在验证后清理。

## 12. 1.5.11 身份与 Stravia labels 投影（2026-11-19）

对本机官方 `agy` 1.5.11 二进制做离线版本检查与只读静态分析，没有执行官方在线推理。`codeassistclient.userAgent` 在 `0x9267bd2–0x9267c57` 使用内嵌构建值 `1024248524`，Linux amd64 consumer 的完整身份为：

```text
antigravity/cli/1.5.11 (aidev_client; os_type=linux; arch=amd64; cl=1024248524; auth_method=consumer)
```

这补齐第 10–11 节已有 1.5.11 出站观察的构建值证据；不冒充一次新的完整 TLS/HTTP 抓包。连接层夹具保留真实 1.5.8 记录，宿主 shape 未改动。

`GetModelRequestCustomMetadata`（`0x8849dc0`）提供以下可确定规则：

- `request_id = fmt.Sprintf("%s-%d", trajectory.ID(), trajectory.NumGeneratorMetadatas())`。`last_step_index = fmt.Sprintf("%d", trajectory.NumSteps()-1)`；两个接口分别解析至 `0x7b03960` 和 `0x7b026a0`，不是同一个计数。
- 三个使用标志序列化为小写 `%t`。没有前一次生成元数据时均从 false 开始；有前一次、但缺少相应标志时，`used_claude` 为 false，`used_claude_conservative` 和 `used_non_gemini_model` 保守为 true。当前 `ModelInfo.api_provider = API_PROVIDER_ANTHROPIC_VERTEX`（26）将前两项置 true；`ModelInfo.model_provider != MODEL_PROVIDER_GOOGLE`（4）将第三项置 true。
- `model_enum` 调用 `Model.String()`。可选 cascade/root/parent 标识只在非空时加入。外层 `computeRequestID`（`0x925e9c0`）另行构造斜杠分隔的 ID；没有操作标识时采用 request type 与随机 UUID，不等于内层标签。

实现按用户要求使用 **Stravia 自己的事实**，并非伪造原生 CLI 的操作历史：链路根映射 cascade，上下文窗口映射 trajectory；生成序号与步骤索引分别投影 canonical assistant 项数和非 system/developer 项数。非 UUID 的宿主亲和键使用固定域 UUIDv8 表示；缺失身份时不生成随机会话。原生累计 metadata 不在输入中时使用上述保守规则，模型判定只读新同步目录中的 provider enums。完整映射与缺项行为见[插件设计](../design/vendor-plugins.md#antigravity--antigravity)。

另在临时 HOME、agent 目录和空 workspace 内运行本机 `omp 18.22.1`，保留原生系统提示和默认工具，所有模型请求截获于 loopback，不转发 Google。捕获 1 次请求、11 个工具、10,070 字节系统提示；其中没有 `RFC 2119`、`system-conventions`、`system_conventions`、`system-directive` 或 `system_directive`。因此 [#5751](https://github.com/router-for-me/CLIProxyAPI/issues/5751) 的精确触发文本不能解释该输入，未直接套用其 RFC 删除或零宽字符方案；后续改动依据下方本次单变量实测，而非该引用的归因。这个结论仅限该次干净 OMP 输入，不排除其他上游内容规则。

隔离验证：`cargo test --locked -p stravia-vendor-antigravity --lib` 的 40 项测试通过；选择性重建 Antigravity Wasm 后，`oauth_and_inference_enforce_native_wire_at_real_http_boundary` 通过，覆盖实际 HTTP 的 UA、标签字符串格式、下游 labels 排除、目录档位和 429 关联。标签生命周期回归区分生成数与步骤数，并验证重试不改轨迹、跨链路隔离及旧元数据缺失行为。真实 Gateway 的 `antigravity_real_wasm_inference_matches_inference_capture` 也通过，覆盖更新后的 UA 及既有 TLS/HTTP 出站形态。

用户要求用最便宜模型验收。只读当前存量连接及其模型价格，确认 `gemini-3.8-flash` 是有登记价格且支持 infer 的最低价项（输入/输出参考价为每百万 token 0.75/3.75）；Flash Lite 仅支持 search，GPT-OSS 与 orchestrator 价格未知，不能当作零价。读取真实目录确认 Low 档可用后，重放上述本地捕获的原生 OMP 请求，保留 10,070 字节系统提示和 11 个工具，仅选择 Flash Low 并将输出上限设为 128。最终 Wasm 实际发送九个字符串标签、目录 `MODEL_PLACEHOLDER_M320` 和完整 1.5.11 UA，但 Google 仍返回 **HTTP 429**。没有重试、fallback、工具执行、生产写入或部署；该请求由隔离宿主执行，并非完整生产 Gateway，不能据此宣称原 Opus High 已修复。

### 单变量定位与最终验证

用户随后明确授权继续真实对照。固定连接、`gemini-3.8-flash-low`、128 输出上限、11 个原生工具、用户消息、会话标签和 UA，仅改变系统文本；外层 attempt UUID 独立。中性系统提示返回 200 / `OK`，穿插的同一中性对照仍为 200。逐段二分定位出四个能单独触发 429 的完整句子：

```text
You are omp's trusted coding assistant.
Write JSON args as `content` to xd://<tool> via `write`. Invalid args return schema in error → fix/retry.
Read xd://<tool> for full docs + JSON schema before first use.
To commit your changes, use the eval `commit` namespace (`d = await commit.draft()`, then `await d.commit("subject", ["- body"])`), not `git add`/`git commit`: it selects only the lines you and your subagents wrote and never touches other agents' staged or unstaged work.
```

只改第一句仍失败；四句全部采用保留含义的改写后，完整系统提示成功。身份句插入 U+200B、单词内 U+034F、以及每个子词内 U+034F 的三个最小案例仍为 429。[CLIProxyAPI #7428](https://github.com/router-for-me/CLIProxyAPI/issues/7428) 和其 [9a7e1e6 修复](https://github.com/router-for-me/CLIProxyAPI/commit/9a7e1e6b412747729dedecefd7e23bd2e3ee1d3d) 也记录 U+200B 的局限并增加 literal replacement；其 U+034F 方案不能代替这里的实测。未采用全局改写、删除系统提示、移到普通消息或增加重试。

最终实现复用 `wire.rs` 的出站系统文本边界，以精确完整匹配执行四项等义替换，保留旧 RFC 片段处理。`system_instruction_compatibility_preserves_messages_and_tools` 先在旧 Wasm 上失败，重建后通过：覆盖多 part、重复出现、近似文本不匹配，以及普通消息和工具描述中相同文本逐字保留。

最终真实验证直接把**未经 Python 改写的原生 OMP 捕获请求**交给新构建的 Wasm：入站系统文本 10,070 字节，插件处理后出站 10,194 字节，仍为 11 个工具、九项字符串 labels 和完整 1.5.11 UA。Google 返回 **HTTP 200**，实际生成文本为 **`OK`**。含二分、中性对照及最终验证的后续实验共 55 次推理调用（20 次 200、35 次 429），加上前述初始失败共 56 次；没有自动重试、fallback 或真实工具执行。临时 host 最初遗漏 gzip 解压，曾在目录 HTTP 200 后本地失败且未发出推理；补齐其解压能力后才执行上述实验，不将该次宿主失败计作 Google 429。

这证明当前账号、Flash Low 和该原生 OMP 输入的内容兼容问题得到修复，不证明 Google 内部检测机制，也不代表所有 429、附加项目上下文、其他历史或 Opus High 都已覆盖。真实测试使用隔离 Wasm 宿主和生产协议 codec，不是生产 Gateway 全路径或 OMP 收到最终 SSE 的端到端验收。未刷新凭据、写入生产库、导入插件或部署；临时测试源码、临时 gzip dev feature 和 OMP profile 已移除，凭据仅通过进程内存/标准输入传递。

### OMP 上游的对应处理（2026-11-19 核查）

- [#19333](https://github.com/can1357/oh-my-pi/issues/19333) 直接报告同一条 `Read xd://<tool> for full docs + JSON schema before first use.`：官方 `antigravity-cli` ACP 路径和第三方 HTTP 路径均有等义改写后成功的用户对照。Issue 已关闭并标为 `wontfix`，并非正式版本已修复；给出的本地方案是通过 `before_agent_start` 或 `before_provider_request` 扩展钩子，在指定供应商路径改写该句。后续评论也报告过改写再次失效，不能当成持久保证。
- [PR #19570](https://github.com/can1357/oh-my-pi/pull/19570) **仍 open、未合并**，提出把默认身份句中的 `trusted` 删除，并把工具文档句的裸 `xd://<tool>` 改为反引号包裹的 `xd://<name>`。作者报告通过 `antigravity-cli/gemini-3.8-flash` 的真实多轮工具验证；这是作者的验证，不是本仓库复测。最新说明明确撤回渲染后的全局正则和跨包重复改写，仅修改内置默认模板及工具目录生成文案，保留用户自定义模板与源范围追踪。
- [#19609](https://github.com/can1357/oh-my-pi/issues/19609) **仍 open**，与本次相同的 OMP 18.22.1，使用 agy 1.5.10 ACP；报告短提示成功、默认提示失败，并指出只套用 #19333 的旧单句替换仍不足，#19570 不在 18.22.1 中。其具体模型与路径不同，不能替代本次四句验证。
- 历史 [PR #13379](https://github.com/can1357/oh-my-pi/pull/13379) 已合并，曾将 RFC 2119 句子改为含 `keywords` 的版本；[#18706](https://github.com/can1357/oh-my-pi/issues/18706) 后来报告新措辞再次触发，目前仍 open 且标有 `wontfix`。因此存在已合并的局部改写，但不存在这些报告所证明的通用、长期稳定的规避算法。
- [PR #18622](https://github.com/can1357/oh-my-pi/pull/18622) 已合并，因账号受限风险暂停内置 `google-antigravity`。这是 provider 准入策略，不是文本 429 修复；不应与运行官方 CLI 的 `antigravity-cli` 或 Stravia 的 OpenResponses 入口混为一谈。

## 13. agy 1.5.11 labels 生命周期：静态与离线多轮对照（2026-11-19）

> 本节的 Stravia 对照记录的是改造前实现。后续已采用宿主派发 cursor 与 conversation CAS 生命周期状态；现行行为、升级要求和未知历史边界见[插件设计](../design/vendor-plugins.md#antigravity--antigravity)，不将本节原生实验扩张为未验证路径的兼容承诺。

本节补充而不覆盖上述历史实验。离线静态对象是 `/home/chikage/.local/bin/agy`，`--version` 为 `1.5.11`，SHA-256 为 `01b063665007b7098116ac98015954d97ff49b7e3147086d75849c16e952ee0e`；下列地址为该 ELF 的虚拟地址。使用 Go pclntab 符号和 x86-64 反汇编；地址不适用于未经核对的其他版本。动态矩阵使用官方 CLI、临时隔离会话、假 OAuth 和 loopback TLS 假上游，进程置于断外网网络命名空间；响应是合成 SSE 或受控错误，**没有真实上游推理**。真实服务仅作一次已授权的模型目录读取，隔离夹具只保留模型公开描述字段；初始一次 eligibility fixture 不完整、未产生推理的尝试不计入矩阵。矩阵共 11 次有推理的 CLI 启动、18 次本地推理 HTTP：17 次合成 200 和 1 次受控 400。以下是客户端行为，不证明服务端如何解释 labels。

运行入口使用本机 `--help` 已确认的 `--input-format stream-json --output-format stream-json`，逐行输入 `{"event":"user","message":{"content":"…"}}`；跨进程使用 `--conversation <conversation_id>`，通过 `--model` / `--effort` 选择模型。根据 CLI 实际输出的 `step_update`、`result` 与本地截获的 labels 对照计数；接口说明见[官方 Headless mode 文档](https://antigravity.google/docs/cli/headless/)。临时脚本、隔离会话与 TLS 材料在记录完成后清理。

### 身份与两个请求号

- 样本中 `cascade_id = root_cascade_id = CLI conversation_id`，`trajectory_id` 是另一个 ID；普通三轮、同会话跨进程 resume 和切模型时三者保持稳定。观察到的三个 ID 均为 UUIDv4，仅是样本，不证明所有创建路径。分叉、子 cascade、压缩后的 ID 继承或重建**没有动态验证**，不能由同会话结果推断。静态符号包含 `trajectory.(*protoTrajectory).Clone`（`0x7b05380`）、`dbtrajectory.(*dbTrajectory).Fork`（`0x8c01160`）和 `stepcompactor.(*overlay).NumSteps`（`0x8c33580`）；符号存在本身不证明这些路径的身份语义。
- `utils.GetModelRequestCustomMetadata`（`0x8849dc0`）构造内层 `request.labels.request_id = fmt.Sprintf("%s-%d", trajectory.ID(), trajectory.NumGeneratorMetadatas())`，以及 `last_step_index = NumSteps()-1`。`trajectory.(*protoTrajectory).NumGeneratorMetadatas`（`0x7b03960`）和 `NumSteps`（`0x7b026a0`）分别经不同底层接口读取，不能视为同一个计数。
- 外层 `codeassistclient.computeRequestID`（`0x925e9c0`）独立构造斜杠分隔值：有完整参数时格式为 `%v/%v/%v/%v/%v`，某字符串参数为空时为四段，操作 ID 为空时为类型和随机 UUID 两段；类型为空默认 `unknown`。本矩阵完整值形如 `agent/<cascade>/<毫秒时间值>/<trajectory>/<数字>`，末数字在已观察请求中等于请求时的 `NumSteps`，即比 `last_step_index` 大一；这是观察，不将全部调用者或时间字段的静态来源视为已证实。外层和内层不是同一关联号。
- labels 并非固定九项：普通会话第二轮起可见第十项 `last_execution_id`。`GetModelRequestCustomMetadata` 的 `0x884a136–0x884a193` 读取最后一条 executor metadata，`0x884a668–0x884a6c5` 仅在 execution ID 非空时加入该项；`ExecutorMetadata.GetExecutionId`（`0x7935ca0`）确认字段偏移 `+0x58/+0x60`。工具续接的同一 user turn 中不一定新增该项；跨 turn 的观测中它变化，同一 turn 两次续接则相同。它不是请求 attempt ID，也不是上一条 generator metadata 的 ID。

### 生成计数、step 和模型状态

| 隔离场景 | 本地观测 |
|---|---|
| 普通三轮 Google | generator 后缀 `0/1/2`，`last_step_index` `0/2/4`；外层末段 `1/3/5`。三个 used 标记始终为 `false`。 |
| 单次响应含 thought 和 text | 没有因此额外增加 step：两轮为 user 0、agent 1、user 2、agent 3。不能由此推断所有多 part 响应。 |
| 一次生成两个并行 `read_file` 工具 | 请求 generator 后缀 `0/1/2`，step 标签 `0/3/5`，外层末段 `1/4/6`；时间线为 user 0、agent response 1、tool 2、tool 3、agent response 4、user 5、agent response 6。两个 functionResponse 合并在一个 wire content 中，仍是两个原生工具 step。 |
| 同模型或切模型跨进程 resume | 观察到额外 `system_message` step；它不是模型切换专属。故原生 step 不等于用户/assistant 消息数。 |
| 受控 HTTP 400 后 resume | 失败请求 generator 后缀为 `0`，生成 `error_message` step 1；恢复请求后缀已是 `1`，尽管此前无 assistant 消息，且该请求 `last_step_index=3`、带 `last_execution_id`。恢复 CLI 返回合成 OK、退出 0，但 result 状态保留历史 `ERROR`，不能称整个会话恢复为 SUCCESS。内部 HTTP retry 如何计数未验证。 |
| Google → Claude Opus low → Google | 续接 Claude 请求后缀 `3`、step `7`，三项标记均为 `true`；再回 Google 时后缀 `4`、step `10`，仍全 `true`。模型切换前后 ID 保持不变。 |
| 全新 GPT-OSS → Google | 首次 GPT-OSS 为 `false/false/true`；回 Google 后仍 `false/false/true`，后缀 `1`、step `3`。 |

静态上 `GetModelRequestCustomMetadata` 从最近一条 generator metadata 的 labels 继承状态：无前条时三个值均从 false 起；有前条但缺键时 `used_claude` 默认 false，`used_claude_conservative` 和 `used_non_gemini_model` 默认 true。当前 `ModelInfo.api_provider = 26` 将前两项置 true，`model_provider != 4` 将第三项置 true；已为 true 的状态跨模型保留。标记按小写布尔字符串输出。这里的保守缺键分支不等于“只要有 assistant 历史就置 true”；正常 Google 生成的 metadata 有明确 false，下一轮仍为 false。

`AppendGeneratorMetadata`（`trajectory.(*protoTrajectory)` 的 `0x7b03e60`，DB 实现的 `0x8bfe280`）与 `AppendStep`（分别 `0x7b03560`、`0x8bfc360`）是不同操作。失败案例证明计数不能用成功 assistant 项数替代；工具场景证明一次生成和多个 step 也不一一对应。受控 400、成功响应和上述工具续接以外，内部重试、其他失败路径、分叉及压缩是否写入或截断 metadata/step 尚未覆盖，不能外推为每个 HTTP attempt 或每个工具都必然各加一。

### 与现有 Stravia 映射的差异和后续依据

当前设计见[插件设计](../design/vendor-plugins.md#antigravity--antigravity)，实现 `RequestLabels::new` 见 `wire.rs`。宿主 chain 根用于 cascade/root、context window 用于 trajectory，是宿主事实的投影而非原生 conversation/trajectory 生命周期；尤其不能据此声称支持原生 fork 或 compact。canonical assistant 项数作为生成数、非 system/developer 项数减一作为 step 的公式，与上述失败生成及原生 `system_message` 的事实不等价；不能仅用 wire content 数推断 canonical item 数。

当前 `used_claude` 仅看本次 provider，故 Claude → Google 会从 true 回 false；`used_claude_conservative = used_claude || assistant_item_count > 0`、`used_non_gemini_model = !google_model || assistant_item_count > 0`，故纯 Google 的第二轮会把后两项置 true，而原生样本仍 false。插件没有 `last_execution_id` 对应状态；外层使用独立 attempt UUID 的 fallback，也不等同于原生具有操作元数据时的五段 ID。这些都是可核查的语义差异，不代表已证实服务端依赖或要求它们。

建议后续先明确宿主能否耐久保存生成记录、step 和 executor 生命周期，以及 fork/压缩和失败重试的定义，再决定是否投影或省略无法可靠表达的字段；不可由消息数或模型名冒充原生累计状态。本轮仅研究和记录，不修改生产插件、数据库或部署。

## 14. Neon 隐式缓存：原生 TUI、工具续接与 Stravia 现场对照

> 下文的 Stravia 抓包早于外层 `requestId` 对齐。对齐时在同一 `agy` 1.5.11 ELF 核实：`StreamGenerateContent` 于 `0x925ef26` 取 `time.Now`、换算 Unix 毫秒后于 `0x925f027` 传入 `computeRequestID`。新的 CCPA continuation leg 也会通过 `0x929718e` 重新取时钟；重发同一已编码 HTTP 请求体则保留 ID。此证据不覆盖所有 HTTP 状态对应的重试分支。当前 Stravia 使用[五段式派发关联号](../design/vendor-plugins.md#antigravity--antigravity)，未知步骤时仍回退。此前缓存读数不代表这个改动能提高命中率。

现场是 OMP 18.22.2 经 `/v1/responses` 到 Stravia 的 `gemini-4-neon-medium`，不是原生 agy。附件中同一用户轮内进行五次完整推理，第六次请求中断、没有上游响应。前五次输入依次为 `11420 / 19236 / 36750 / 53576 / 60478`，上游缓存读数为 `0 / 0 / 19237 / 0 / 0`。只有第 3 次的 Google SSE `usageMetadata` 带 `cachedContentTokenCount`；其它完整 usage 省略的私有 proto3 标量按零解释，不能将第 6 次也计为零。各次的 Provider、Target、project、实际模型、`prompt_cache_key`、`sessionId`、cascade/root/trajectory、系统指令、工具和生成配置相同；上一请求的所有 `contents` 都逐项原样保留，包含工具参数、结果和签名，仅在尾部追加。附件已永久脱敏 Authorization，不能比较其原值。第 3→4 次派发相隔约 20 秒，且输入增加 16,826；这不是输入不够长、旧前缀被改写或可见选路变化的例子。

使用官方 agy 1.5.11 的交互 TUI、隔离 CLI profile 和 loopback HTTPS 观测器。Mock 在断外网的网络命名空间执行，`read_file` 只读合成文件；真实探针只转发 Neon Medium 的 Agent 推理，预取标题等辅助生成和 ACL 写入在本地处理，不重放事故内容。真实对照共 20 个 Agent 请求，均 HTTP 200；靠读取 Google SSE 而不是 TUI 输出计量。

| 对照场景 | 输入数（依次） | Google 缓存读数（依次） |
| --- | --- | --- |
| 原生普通五轮，均匀追加 | 17068 / 20208 / 23355 / 26500 / 29634 | 0 / 0 / 20209 / 23356 / 26501 |
| 原生普通五轮，接近事故量级 | 11716 / 19358 / 35928 / 51984 / 58807 | 0 / 11717 / 19359 / 35929 / 51985 |
| 原生同一用户轮，四次顺序 `read_file` | 9747 / 16094 / 22286 / 28470 / 34626 | 0 / 9748 / 16095 / 22287 / 28471 |
| 同一工具实验，观测器只将 `request.sessionId` 改成会话内稳定的非空输入散列 | 9742 / 16040 / 22232 / 28416 / 34572 | 0 / 0 / 0 / 22233 / 28417 |

- **原生 `sessionId`**：同项目、同档位的原生样本值等于空串的 FNV-1a64 按有符号十进制输出（`-3750763034362895579`），并非 cascade 或 trajectory UUID 的散列。`getSessionID`（1.5.11 ELF `0x925b060`）证实算法；不能外推至所有 CLI 路径。Stravia 的非空版本由[宿主链路根、主体、Target 派生](../../backend/crates/stravia-core/src/model_turn/live.rs#L1955-L1967)，再经[同格式 FNV-1a64](../../backend/crates/stravia-vendor-antigravity/src/wire.rs#L77-L87)；现场值全程不变且非空串散列。改写探针在转发前断言仅一项 JSON 字段变化；后两轮命中，说明稳定的非空散列并不禁用缓存。两组工具实验是独立会话，时序、模型输出与上游内部状态不固定，样本不足以从早期未命中归因于 `sessionId` 或估计命中率差异。
- **原生会话与关联号**：同一会话的 `labels.cascade_id`、`root_cascade_id`、`trajectory_id` 稳定。`labels.request_id`、`last_step_index` 和外层 `requestId` 随生成变化；原生外层五段 ID 中含 cascade 和 trajectory，现场插件为 `agent/<attempt UUID>`。同一用户轮工具续接时，原生和现场均可全程没有 `last_execution_id`；跨用户轮才观察到它变化。关联号不是已知 Google 缓存键。原生样本没有 `cachedContent`、`prompt_cache_key` 或专用缓存 HTTP 头。
- **并行工具历史打包**：mock 返回两个原生 `read_file` 调用，下一请求将两个 `functionCall` 放进同一个 `model` content（首个带签名），两个 `functionResponse` 放进同一个 `user` content。现场 Stravia 回填也合并 `functionResponse`，但将每个并行 `functionCall` 放进不同 `model` content。两种路径各自都只在末尾追加，故差异是历史表示而不是轮间改写。未在真实上游单独改变并行分组，不能将它归为已知失效原因。
- **Stravia 内部亲和**：入站 `prompt_cache_key` 可用于 Conversation Affinity，不原样送给 Google；当没有会话身份时才可能使用 [canonical 前缀的 Cache Affinity](../../backend/crates/stravia-core/src/router/cache_affinity.rs#L63-L162) 偏好已成功处理前缀的 Target。其 20,000 输入阈值只影响 Stravia 选路，不决定 Google 是否命中；本次没有切 Target。

[Gemini 公开缓存文档](https://ai.google.dev/gemini-api/docs/caching?generate-content=true)给 Neon 4,096 输入门槛及 Gemini 4 完整前缀复用的 2,050 新增输入条件，不保证隐式命中，也没有公开 Antigravity 私有服务的键、落盘状态、分片或失效原因。原生出现过续接未命中，且纯文本能稳定复用约 52k、工具续接也能命中；没有在原生探针复现现场“命中后连续归零”。现场最窄结论是上游没有报告重用保持不变的前缀；不能断言具体是未写入、淘汰、分片或某个请求形状差异。后续优先用同一合成并行工具 workload 隔离真实 Gateway 与原生路径的差异；复发时用第 3–5 次 SSE envelope 的 `traceId` 向 Google 核对 cache write / lookup。在因果证据前不改会话键、不重试补缓存，不将真实提示、追踪号或凭据写入此笔记。



