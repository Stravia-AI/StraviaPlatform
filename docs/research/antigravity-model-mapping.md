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

插件当前不消费上述默认项、分组、用途清单和废弃重定向，也未读取 `maxOutputTokens`、`recommended` 等所有详情字段。尤其截图中的 Image 命名与文本输出图标并不矛盾：这是当前投影限制，不能证明该模型实际不支持图像生成。

宿主元数据模板优先级为 Provider Catalog scope、Canonical fallback、bare discovery；之后 discovery 同名字段覆盖模板，并强制保存原始发现 ID。Canonical 只补规格，不改写上游调用 ID。依据：[provider_model_records.rs](../../backend/crates/stravia-core/src/admin/routes/provider_model_records.rs)，619–687、736–803 行；[provider_catalog/mod.rs](../../backend/crates/stravia-core/src/provider_catalog/mod.rs)，678–709 行。

未被用户接管的 Imported 记录跟随来源刷新；Edited 保留规格，仍刷新插件托管字段；Manual 记录跳过。新增/缺失/恢复按 ID 协调，不按名称去重，缺失记录保留而不是删除。实现依据为上述 `RouteModule::sync`，不能只依据较旧 [ADR-0054](../adr/0054-persist-provider-model-snapshots.md) 的“Imported 保持不变”表述；该处文档与当前实现存在偏差，本次不修改其决策内容。

推理代码直接将 Target 的 `provider.model` 放入 envelope `model`，没有将 `gemini-pro-agent` 改为 `gemini-3.1-pro-high`，也没有反向改写：[wire.rs](../../backend/crates/stravia-vendor-antigravity/src/wire.rs)，34–42、149–159 行。

## 3. 截图同名模型与建议

截图只能证明界面有两条同名不同 ID 的记录。独立运行当前解析器的合成输入证明，即使两条记录具有相同 `displayName` 和相同 `model` enum，它们仍保持两个 ID，`selector=None`。这不是截图账号的原始响应验证。

官方 [Models 文档](https://antigravity.google/docs/models.md) 展示 Gemini 3.1 Pro 的 High/Low 选项，但不公布上述两 ID 之间的别名关系。新增的直接证据来自 CLI 二进制，而不是该页面或第三方代理：`backend.map.init.0`（`0x8316300–0x8316540`）将 `gemini-pro-agent` 登记为 `{base: "gemini-3.1-pro", effort: "high"}`；`slugFor`（`0x831bdc0–0x831be60`）先查该映射，`UserFacingSlug`（`0x831c000–0x831c080`）连接 base 与 effort，得到 `gemini-3.1-pro-high`。

若目标是对齐官方 Agent 选择器，应从响应的 `agentModelSorts[].groups[].modelIds` 构建选择分组，结合 `defaultAgentModelId`、真实 `deprecatedModelIds` 与官方 user-facing slug。实际请求 ID 和显示 slug 必须分开保留，不能因为 slug 映射就擅自替换上游请求 ID；也不能按同名或同 enum 合并。完整账号模型目录可以另行保留。用途清单和 Agent 清单应区分。是否改变 Stravia 的展示/选择/执行契约属于后续产品决策，本次没有实施。

## 4. 实际验证

1. `task build:vendors:all`：成功生成八个自包含 Wasm 组件。
2. `cargo test --locked --jobs 4 -p stravia-vendor-antigravity --test contract oauth_and_inference_enforce_native_wire_at_real_http_boundary -- --exact --ignored --nocapture`：1 passed；实际加载 Wasm，经过 loopback HTTP 发现账号 ID、过滤 internal，并验证 OAuth、项目 operation、推理协议边界等。没有连接 Google。
3. 临时 `cargo run --locked --jobs 4 -p stravia-vendor-antigravity --example model-mapping-smoke`：包含原始 catalog/client/auth 模块，给解析器传入合成双 ID 同名目录；输出保留 `gemini-3.1-pro-high` 与 `gemini-pro-agent` 两条，context 均为 1048576，模态均为 text/image/video 输入、text 输出，selector 均为 None。临时入口首次缺失 client 模块而编译失败；补齐原模块后运行成功，之后已删除入口，没有增加永久测试。
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

推理根据 Target 的 Effort 从选择表取真实 ID；缺省使用该家族的默认 selector，未登记 Effort 在 HTTP 前返回明确错误，不生成假想 ID 或静默改档。已消费的 Effort 不再同时编码为 Gemini thinking budget。已有逐档记录与 Route 绑定不自动删除或迁移，部署步骤见 [插件设计说明](../design/vendor-plugins.md#antigravity-cli-接入)。

实际验证：

- `cargo test --locked --jobs 4 -p stravia-vendor-antigravity --lib`：15 passed。覆盖 Agent 引用筛选、其他用途排除、默认档位、重复引用、同名身份分离、真实重定向与循环拒绝、规格交集及未登记 Effort 拒绝。
- `task build:vendors:all`：增量重建 Antigravity，生成八个自包含组件。
- `cargo test --locked --jobs 4 -p stravia-vendor-antigravity --test contract oauth_and_inference_enforce_native_wire_at_real_http_boundary -- --exact --ignored --nocapture`：1 passed。实际 Wasm 与 loopback HTTP 验证默认/Low/Medium/High 的 envelope ID、无双重 thinking 控制，以及未登记 Effort 不发送 HTTP；原有 OAuth、签名、SSE 和错误边界仍通过。
- `cargo clippy --locked --jobs 4 -p stravia-vendor-antigravity --all-targets -- -D warnings`：通过。
- 在全新临时 SQLite 实例导入实际构建的插件，重放第 6 节响应中已去除账号信息的模型字段。OAuth 测试状态直接在隔离存储中预置假 token 与假 project；没有重新登录或读取真实账号凭据。真实同步返回 `added=7`，模型 API 输出七个家族。浏览器实际点击添加 Pro 家族，保存的 Route 仅支持 `low`、`high`；Flash 家族支持 `low`、`medium`、`high`。
- 经真实 Server 的 `/v1/chat/completions` 向 loopback 上游发送请求：Pro 缺省与 High 为 `gemini-pro-agent`，Low 为 `gemini-3.1-pro-low`，Flash Medium 为 `gemini-3.8-flash-medium`；校验上游捕获的 envelope 与客户端响应一致。
- Chromium 实际页面核对英文深色桌面表格、中文浅色 390px 移动列表，Pro 只有一行并显示 `low, high`，Flash 同行显示三档；未显示 Gemini 2.5 Pro 等非 Agent 项，浏览器无错误。复用现有 Svelte 页面，没有修改前端源码。

临时服务、存储、模型响应夹具与截图在验证后清理。上述请求只访问本地假上游，不证明真实 Google 推理可用性；未运行完整仓库测试矩阵或 Tauri 原生宿主。本轮首次编译的错误枚举名称已修正为项目既有 `ErrorKind::Invalid`，后续列出的检查均为修正后结果。

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
