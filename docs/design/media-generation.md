# 媒体生成

本文记录媒体生成的功能契约，不作为验证结果报告。架构取舍见 [ADR-0061](../adr/0061-bind-media-generation-to-routes.md)，领域术语见 [CONTEXT.md](../../CONTEXT.md)。

## 1. 范围与入口

Media Generation 的用户可见名称为“媒体生成”，属于 Advanced Capability。统一工具名为 `generate`；本期只支持图片生成与参考图编辑，首个接入后端为 Codex，不提前公布音频或视频类型。

当前接入 Codex 托管图片工具和 Antigravity 原生图片模型。工具通过平台执行，支持显式调用、MCP 及可选透明注入。媒体生成使用独立平台总开关，关闭时所有入口均不可执行。总开关开启后，所有有效 API Key 可显式调用；MCP 另受既有 MCP 开关约束。API Key 增加“媒体生成”透明注入选项，新建及已有 Key 均默认不勾选；该选项只控制自动暴露，不是能力授权。沿用 [ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md)。

透明注入总开关开启时同时提供基础 `StraviaRead`：所属 Artifact 的下载及公开工具执行状态／原始结果查询不依赖联网、多模态理解或媒体生成选择。联网与媒体理解仍按各自 Gate 和注入选择限制，不因基础 Read 自动开放。

## 2. 工具输入

按 `type` 与 `input` 建立判别联合，不采用任意 JSON 参数袋，不将不同媒体类型的所有字段平铺为可选项。

```ts
type GenerateInput = {
  type: "image";
  input: {
    prompt: string;
    aspect_ratio?: "1:1" | "3:4" | "4:3" | "9:16" | "16:9";
    resolution?: "1K" | "2K" | "4K";
    reference_images?: string[];
  };
};
```

- `type`、`input`、非空 `input.prompt` 必填；未知类型和未知字段明确拒绝。
- `aspect_ratio`、`resolution` 为生成偏好，不保证精确输出；省略时采用当前后端默认行为。
- 比例、分辨率档位及上游尺寸字段的转换由对应 Provider 的媒体生成适配实现拥有，允许近似映射。统一工具不维护供应商映射表，不新增 `image_size` 或供应商专用字段。
- `reference_images` 省略或为空表示文字生图；有参考图时按提示词执行编辑或参考生成。数组顺序对应“图 1、图 2……”；不得静默忽略或截断参考图，超出后端能力时明确失败。
- 不接受工具级 `model`、`provider`、`route`、`count`、`action` 或历史图片数量索引。每次成功返回一张图片。
- 未来新增类型时增加其专属输入分支；不要求音频、视频沿用图片的 `prompt` 或参考图角色，也不改变已有图片字段语义。

结构示例（引用 ID 为占位，不是有效调用值）：

```json
{
  "type": "image",
  "input": {
    "prompt": "保留参考图中的小屋，将背景改为雪山，水彩风格",
    "aspect_ratio": "16:9",
    "resolution": "2K",
    "reference_images": ["stravia://artifacts/<artifact-id>"]
  }
}
```

## 3. 文件输入与输出

参考图接受所属 Principal 的 Artifact Reference（`stravia://artifacts/<artifact-id>`）或公网 HTTP(S) 图片 URL，不接受客户端本地路径。客户端本地文件先通过现有上传入口取得引用；工具不另设 base64 或上传参数。

- Artifact Reference 必须校验当前 Principal 的归属、保留期及图片内容可用性，引用本身不授予访问权。
- 公网图片 URL 必须沿用既有网络安全与大小限制，先收存为当前 Principal 的 Artifact，再开始上游生成；收存失败不透传原 URL、不忽略附件继续执行。
- 输入是图片来源，不承载 `StraviaRead` 的 `question`、`download` 等操作选项。
- 向 Provider 交付输入内容时复用现有媒体传输规则，不把 `stravia://` 引用直接交给上游，也不将客户端或服务端本地路径作为上游可读地址。
- 上游返回的图片必须完整收存为当前 Principal 的 Artifact 后才能成功。上游 URL、base64 和本地存储路径不是工具的公开产物身份。

成功结果：

```ts
type ImageGenerateOutput = {
  path: string; // stravia://artifacts/<artifact-id>
  mime_type: string;
  size: number; // 文件字节数
  download_url: string; // 15 分钟 bearer 下载授权，不是稳定身份
  media: {
    width: number; // 实际像素宽度
    height: number; // 实际像素高度
  };
};
```

`media.width` 与 `media.height` 从实际生成文件读取，不以请求参数代填。结果不重复返回裸 Artifact ID，也不重复返回推算比例或分辨率档位。其他媒体类型的 `media` 字段随对应能力定义。

新生成结果直接附带 `download_url`，不增加 `expires_at`。它使用所属 Principal 的既有 15 分钟 bearer 下载授权，固定从配置的完整 `client_base_url`（含协议、端口及部署路径）经过平台读取实际存储文件，即使开启外部签名下载也不改用 S3 或文件公开入口。此规则不改变 Provider 交付及 `StraviaRead({path: "stravia://artifacts/<artifact-id>?download=1"})` 的既有下载策略，也不回写历史结果；异步执行查询原样保留生成结果中的链接，不自动续期或重新生成。

工具描述要求用 `![图片描述](download_url "stravia://artifacts/<artifact-id>")` 展示结果，代入实际链接和稳定 `path`；title 在链接失效后保留产物身份，不代表自动续签或重新生成。有效链接的展示不要求额外调用 Read，稳定 `path` 仍用于后续参考图复用。

调用上游前必须校验客户端入口非空且有效。图片已落盘但链接签发失败时，工具返回 `error.code = "artifact_download_failed"` 及 `artifact`（包含原 `path`、`mime_type`、`size`、`media`），保留图片与 Artifact，不重跑生成；调用方可使用保留的引用获取新的下载授权。保留期、归属、上传与下载授权沿用 [ADR-0048](../adr/0048-separate-artifact-references-from-transfer-grants.md)、[ADR-0049](../adr/0049-snapshot-explicit-media-inputs-as-artifacts.md) 和 [ADR-0051](../adr/0051-disambiguate-artifact-download-and-understanding.md)。

### 公开工具的等待与执行引用

`generate` 与 `StraviaRead` 的 MCP、显式调用及透明注入共享 Core 执行模块。工具元数据可声明同步等待窗口，未声明时为 60 秒；等待包括平台排队与输入准备，不改变原有 15 分钟执行期限。窗口内完成继续返回原结果或原错误，不强制增加任务封装。内部 Agent 的基础读取保持同步内容契约。

窗口耗尽且后台交接已持久化时，返回 `{"path":"stravia://tool-executions/<id>","status":"pending"}` 或 `running`。执行引用标识一次平台执行，不是模型 Tool Call ID、History Marker 或 Artifact；它不代表图片已生成。交接前原调用断连或取消会取消本地执行，交接后执行独立继续，即使引用未能发送给客户端。交接存储失败明确报错，不返回未持久化的引用。

交接使用显式事务：等待数据库锁及更新期间仍可由原请求取消；不可逆 `COMMIT` 前在短同步临界区检查取消事实，已取消则回滚。进入提交后，断连与尚未决议的提交属于并发操作，由实际事务结果收敛，不把 ACK 返回后的调度先后当作物理落盘时刻。成功提交不能被晚到取消推翻；提交报错时，若读回能确认已经持久交接则保留后台执行，否则明确报告存储故障，不声称已经回滚，也不自动重放。原执行期限和 Gateway 关闭仍可中断执行；来源诊断清理不拥有执行生命周期。

隐藏调用的 History Marker 必须先持久化、实际交付并经 Sent 确认后 publish，之后才能启动领域工作。Marker 发布不是后台交接：交接前原请求断开且取消获胜时，即使 Marker 已发布，也保存原调用与明确的取消 tool output；恢复不依赖旧 Hook 会话，不改成笼统的交付处理错误。已完成的终态不被随后断线改写。

领域最终结果与 Marker 的本次交付结果分别持久化。常规交付由所属 Inference Run 的 ToolResult Hook 处理后封存；交付的是执行引用时，Marker 以后仍恢复该次引用，不随后台完成而替换。除系统确定的取消结果外，原请求消失导致 Hook 无法完成时，Marker 保存明确交付处理错误，不放行原始成功结果；这不取消已交接工作，也不改写其独立最终结果。后台完成不调用旧 Hook，后续查询输出由查询所属请求的 Hook 处理。

用 `StraviaRead({"path":"stravia://tool-executions/<id>?wait=120"})` 查询。省略 `wait` 或设为 `0` 立即返回；正整数表示本次查询最多等待的秒数，可超过初次 60 秒窗口，但不得越过查询请求与原执行的剩余期限。完成会提前唤醒所有等待者；取消查询只停止自己的等待。重复查询不重新执行、不创建后台查询任务、不延长执行或保留期。`wait` 只适用于执行引用，不新增顶层参数；非法值、重复和未知选项明确拒绝。

未完成时返回稳定 `path` 与真实 `pending` / `running` 状态；完成时返回 `{"path":"…","status":"completed","result":<原工具结果>}`。原结果中的 Artifact、报告与分页引用继续按原规则消费。原任务失败或进程重启中断时，查询本身成功，返回 `failed` / `interrupted` 与 `error`，不设置本次读取的工具错误标志。未知、过期、被清理或不属于当前 Principal 的引用是查询错误；未知与其他 Principal 的引用不泄露存在性。

已完成结果在保留期内跨重启可读；重启中断未完成执行，不自动接管或重放，旧 owner 或迟到完成不能覆盖已确定的终态。MCP 与 Inference Run 的公开执行均独立于来源诊断：诊断到期、手动清理、缺失或写入失败不取消已交接工作、不使有效引用失效。运行中沿用原始 deadline；完成、失败或被判定中断时，从该时刻按现有历史保留时长独立保留结果，不借用来源诊断剩余寿命，不因查询续期，也不新增 TTL 配置。

清理执行引用不删除独立 Artifact；Artifact 仍可能按自己的保留期先到期。取消仅保证本地停止和迟到提交隔离，不保证供应商已停止或不再计费。后台任务保留创建时的 Principal、RootRequest、权限、路由、安全和 RPM 归属。

启用 Debug 时，执行同样保留来源 Run / Trace：Agent 或其他请求内调用工具，其参考输入的远程读取及供应商收发写入来源请求的诊断包；转后台不建立新的工具包。无父请求的独立 MCP 调用才建立独立观测。Debug 导出保留既有固定截止水位，后台完成后的内容需要重新导出；历史缺失字节不补录。

SQLite / PostgreSQL 的增量迁移 `0024_independent_tool_execution_retention.sql` 在既有公开身份与结果之上，移除来源删除触发器、清理 fence 与来源 lifetime 计算，保留执行 owner／lease、deadline 及终态不可覆盖保护。升级前仍可查询的旧终态结果保留原引用与结果，从迁移时刻获得完整的新保留窗口；迁移起点单独记录，未知的实际完成时间保持未知。已不可查询或已删除的结果不复活、不补造。升级前停机备份完整数据根与外部数据库；回退必须恢复备份，不能以旧二进制打开升级后 schema。

## 4. Route 与 Provider 接入

管理员按生成类型分别绑定 Route，当前仅有图片绑定。调用方不能覆盖该绑定。Route 持久化单一的 `output_modality`，当前为 `text` 或 `image`；新建表单默认文字，管理创建请求必须显式提交用途。Provider Model 可以同时登记文字和图片输出，能接收图片不等于能生成图片。这一用途约束取代先前不引入 Route 输出类型的限制，但不恢复已移除的协议 dispatcher route-type gate，也不改变普通推理与托管图片生成的协议含义。

候选及写入资格以 Provider Model 原始持久化快照的输出登记为准；缺失模态、缺失输出、显式空输出列表和仅登记其他输出均不赋予文字或图片资格。既有读取场景的文字回退不是能力登记；准备流程已实际保存的文字输出则按真实登记处理。Effective Availability 等既有约束继续生效，强制可用不能绕过模态要求。Core 统一校验所有管理入口；新增绑定和切换用途时，所有提交保留的 Target（包括禁用项）均须匹配。允许同次替换失配项，不自动删除绑定，失败不更改原配置且编辑器保留草稿。

已有 Target 的读取投影提供 `route_output_modalities`，由 Core 解析其模型身份（含历史别名）后按原始登记计算，不与当前可用性混合；新候选的 Provider Model 摘要仍同时约束可用性。历史 Target 可在保留原模型标识的情况下明确保存匹配用途，不要求客户端复制别名匹配或重写绑定。

只有 OneClick（一键接入）可为新 Route 推断用途：登记包含文字时优先创建文字路由（双输出也只创建一条），否则包含图片时创建图片路由，两者均无则拒绝。不添加 Route ID 后缀；已有 Route 以其已配置用途为准，不能通过重复绑定绕过待配置或失配检查。显式绑定只向已有 Route 增加 Target；Route 不存在时返回 `ROUTE_NOT_FOUND`，不隐式创建或推断用途。

图片生成绑定只接受已配置为图片、已启用且有可执行 Target 的 Route，并在候选读取、配置保存和每次执行前校验。图片输出声明不能替代真实媒体生成适配器资格。后续规格变化导致输出模态失配时，保留 Target 并显示原因，执行时先将其排除；其余合格 Target 继续按既有调度、重试和切换策略执行，全部失配或无可执行 Target 时在上游发送前明确失败。登记修复后原绑定可恢复资格。仅输出模态失配采用该排除规则：对剩余已启用 Target 仍执行原有适配器检查，其他适配器不兼容仍使媒体路由校验或执行失败，不静默跳过；禁用 Target 不参与该适配器检查。

输出失配排除必须先于原生图片适配器的优先选择，避免失配的原生图片 Target 压制仍合格的托管图片工具桥接 Target。`media_generation_targets_output_modality_mismatch` 表示应修复输出登记或替换失配 Target；它不是启用状态错误，不应引导管理员重复启用已经启用的 Route 或 Target。

媒体生成配置对 Route 的引用即使总开关关闭也阻止将用途改为文字。必须先清空图片生成 Route 引用；管理页支持解除绑定并同时关闭功能，关闭总开关或删除 Target 均不等于解绑。失效旧引用保留供修复，仍允许关闭功能或明确解绑，不将管理员锁在无效配置中。

**升级迁移：SQLite 和 PostgreSQL 的 `0022_default_existing_routes_to_text.sql` 将全部未配置输出用途的历史 Route 统一设为文字，已明确配置的文字或图片用途保持不变。** 它覆盖尚未应用和已经应用 `0021_route_output_modality.sql` 的实例，不改写旧迁移 checksum。Route ID、展示名、Target、启用偏好、优先级、超时、重试及其他调度参数和图片生成引用均保留；不自动启用禁用项，也不修改 Provider Model 规格或绕过资格校验。历史图片生成引用若指向被回填为文字的 Route，须明确改为图片用途并修正失配目标，或更换、清空引用。重启不会重置已保存用途。升级前停机备份完整数据根和外部数据库；回退须恢复备份，不能让旧二进制打开升级后 schema。冻结基线和增量升级规则见 [ADR-0073](../adr/0073-cutover-to-single-baseline-schema.md)。

按照已确认的 [Vendor 插件设计](vendor-plugins.md)，媒体生成可以与推理、完整搜索由同一 Vendor Plugin 和 Provider 连接提供，也可以是插件唯一的能力。沿用 Provider Model → Target → Route；Provider Model 可表示纯图片模型，不以聊天能力为前提，普通聊天执行不得选择不具备其所需能力的 Target。

插件拥有供应商参数映射、请求及结果解析；宿主继续拥有公开 generate 工具、权限、输入处理、Artifact 收存与交付，不允许插件自行注册 MCP 工具。能力移除更新保留绑定并明确不可用；内置 `base` 自动更新不为此暂停确认，不兼容数据按格式版本差异自动重置；本地包更新和手动恢复仍须确认数据丢弃。兼容在途生成使用旧版本完成，不兼容更新取消生成并阻止迟到结果提交。

Codex 参考 [OMP image-gen.ts](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/tools/image-gen.ts) 的实现方式：

1. 通过 Route 选择 Target，复用相应 Codex Provider 的账号、认证和连接设置。
2. Target 上游模型须明确具备所选 Responses 托管图片工具能力，不按型号名称或版本猜测。
3. 调用 Responses，以 `tool_choice` 强制选择 `image_generation`；参考图决定生成或编辑操作。
4. 消费流式响应，提取图片，收存并返回 Artifact 引用及实际规格。

Codex 模型发现只接受来源明确的 `media_image`、`output_modalities` / `modalities.output` 中的 `image`，或 `experimental_supported_tools` 中的 `image_generation` 作为图片资格；不再按模型前缀、主版本或静态型号清单赋予能力。已有真实文字及其他输出保留，显式空输出不补成文字；图片专用型号不因缺省填充获得聊天能力。生成执行同样不做型号名称拒绝，沿用宿主对所选 Target 的资格校验并原样发送其模型身份。

公开 [Codex ModelInfo](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/openai_models.rs) 的 `input_modalities`、`supports_image_detail_original` 仅说明输入，不能作为图片输出证明；未找到可替代上述明确声明的动态账号图片资格接口。OMP 的 [hosted-image 规则](https://github.com/can1357/oh-my-pi/blob/main/packages/catalog/src/compat/rules/classes/openai.kdl) 仍包含静态 lineage/version 推断，不能冒充上游动态能力。缺少证明表示资格未知，不表示服务端一定不支持；先前按名称登记的候选可能在同步后退出图片列表。仅含模型 ID 的静态清单也不自动授予图片输出，需要来源或管理员明确配置模型能力。

更新并重新导入 Codex 插件后，已从来源导入且未经人工编辑的 Provider Model 在下次同步时刷新登记；人工编辑的规格继续由管理员接管，不强制覆盖。目录声明、历史成功调用与当前账号授权是不同事实，不以固定名单维持过时资格。

不运行 Codex CLI，不采用独立的 `images/generations` 或 `images/edits` 调用链，不切换为另行配置的 OpenAI API Key。实际支持的模型、参数映射及错误分类须以选定链路的源码与协议证据核定；目录声明不能代替接入资格验证，保存配置也不代表已经执行过真实生图验证。

### Antigravity 原生图片生成

Antigravity 参考 OMP 的 [原生图片适配器](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/images/google-antigravity.ts) 和 [图片模型发现](https://github.com/can1357/oh-my-pi/blob/main/packages/catalog/src/discovery/antigravity.ts)，不借用普通推理请求或 Codex 托管工具：

1. 模型同步读取账号 `fetchAvailableModels` 返回的 `imageGenerationModelIds`，解析已声明的废弃 ID 重定向并过滤禁用目标；图片模型保留真实 ID，不按推理档位折叠，不创建虚拟模型或固定默认型号。广告 ID 缺少详情时保留该模型，但不猜测上下文、图片输入或推理能力。
2. 图片专用模型登记 `media_image` 和 `modalities.output: ["image"]`；同一 ID 已有 Agent 推理能力时保留文字输出与推理选择表，并增加图片能力。`supportsImages` 只声明图片输入，不能据此赋予图片输出。
3. 通过图片 Route 选择 Target，复用对应 Provider 的 OAuth、project 和连接设置。调用 `streamGenerateContent?alt=sse`，上游模型取 Target 的真实 selector，不在生成时另选账号广告列表中的首个型号。
4. 原生请求使用 `responseModalities: ["IMAGE"]`、`candidateCount: 1`；有序参考图编码为 `inlineData`，比例与分辨率直映射到 `imageConfig.aspectRatio` 和 `imageConfig.imageSize`，省略的偏好不补默认值。普通推理的参数白名单保持不变。
5. 消费 SSE 的图片 `inlineData`，要求成功终态及恰好一张图片，限制解码图片为 32 MiB；空图、非法编码、非图片 MIME、多图、截断或上游错误均不报告成功。Core 继续完整解码图片、校验实际规格并收存为 Artifact；HTTP 和流内错误沿用 Route 错误分类与重试策略。

已安装实例须重新构建并导入独立 `antigravity` 插件，再同步账号模型，随后将发现的图片模型绑定到图片 Route 并在媒体生成中选择该 Route。来源托管的规格可随同步刷新，人工编辑的规格不被强制覆盖；不自动创建、改绑或启用 Route。账号是否广告图片模型、配额与真实生图资格仍由上游决定，本地合约验证不能代替真实账号验证。

## 5. 失败与重试

沿用普通推理的可重试错误分类与 Route 重试、Target 切换策略，参见 [ADR-0034](../adr/0034-layer-route-target-selection.md)。不因请求已发送且执行状态不明而额外阻止原策略允许的重试；明确接受重复生成、重复消耗额度的风险，不承诺恰好执行一次。

- 不将参数校验、参考图读取或收存失败归类为可重试上游错误。
- 不将用户取消重新归类为重试条件；已有输出提交边界继续适用。
- 上游没有有效图片、输出图片无效或 Artifact 收存失败，均不能报告成功。
- 输出图片已生成但本地收存失败时，不以恢复文件保存为由重新调用生成。
- 使用现有工具错误通道，不返回 `success: true` 的空图结果，不把失败伪装为异步任务成功。
- 上游明确给出的失败原因经既有凭据结构和本次执行已注册凭据脱敏后，保留在工具结果的 `error.message` 中，不统一替换为泛化的生成失败文案；既有 `error.code`、取消及超时语义不变。认证错误和 HTTP 401/403 仍返回安全摘要，不公开凭据诊断。MCP、请求内平台调用及后台执行查询共享同一结果。
- Antigravity 的明确内容政策拒绝按 `ContentFiltered` 结束，不重试、不切换 Target，也不改变共享健康失败计数。解析同时覆盖结构化拒绝字段及只有明确 Google 政策拒绝 `finishMessage`、没有 `finishReason` 的响应，并保留公开拒绝原文。其他非成功终态保留其 `finishMessage` 或原因码；未知说明或真正截断仍按原有流错误处理，不把所有无图响应视为内容过滤。

## 6. 管理面范围

遵循根 [DESIGN.md](../../DESIGN.md)，在“高级功能”中提供“媒体生成”配置页。本期仅提供能力总开关、图片生成 Route 选择与解绑、配置校验与当前支持类型的说明，不提供手动试生成、参考图上传、图片预览或图库。

图片路由选择是即时保存：选定完整有效的路由后立即写入，成功不弹 toast，也不出现独立的「保存设置」按钮；保存失败时选择回到服务器确认值，并在原地显示可重试的错误。已保存路由旁提供「解除图片路由」按钮，打开确认 Sheet 写明影响，保存后清空 `image.route_id` 并同时关闭媒体生成，即使当前功能已开启也允许保存。没有可用候选时说明原因并提供模型管理入口，不回显重复的资格说明。配置校验问题通过启停开关上的 Tooltip 按需显示，不再常驻告警卡片或管理快捷按钮；支持悬停、键盘聚焦、触摸查看与 Escape 关闭，配置阻止启用时仍可聚焦并读取原因，保存有效路由后不再显示该提示。配置失效但功能仍开启时，仍允许关闭。加载失败与支持的媒体说明保持不变。

路由保存成功后刷新模型列表与详情的绑定状态，模型编辑器依据最新读取的引用判断占用，不把进入页面时的绑定快照当作当前事实，也不因刷新覆盖编辑草稿。

实际生成通过客户端工具调用、透明注入或 MCP 执行，不以管理员身份另建生成及 Artifact 所有权路径。配置不足必须说明缺失条件与补齐入口；配置保存、功能启用与真实请求成功是不同事实，不自动发送可能收费的上游测试请求。

## 7. 验收范围

实施时须通过产品入口验证以下已确认契约；本节是验收要求，不是已执行的测试报告：

- 统一输入严格按类型校验，图片生成与参考图编辑可经 Codex 托管工具或 Antigravity 原生图片模型完成；Antigravity 调用所选 Target 的真实图片模型，而非自动改选广告列表首项。
- 图片 Route 的绑定与运行前资格均校验；输出模态漂移保留绑定但排除执行，剩余合格 Target 可继续执行，其他适配器不兼容仍明确失败。
- SQLite 和 PostgreSQL 升级后，未配置用途的历史 Route 均自动归为文字；已明确保存的用途、身份、绑定和启停偏好保持不变。失效图片生成引用可关闭、解绑，关闭总开关不等于解绑。
- 平台 Gate、显式调用、MCP 与透明注入组合符合既有高级能力规则，已有 Key 不自动启用媒体生成注入。
- 文件归属、收存、保留期及签名下载沿用 Artifact 契约；无跨 Principal 引用和本地路径泄漏。
- 实际图片像素与返回的 `media` 一致；近似映射不伪报原请求尺寸。
- 错误分类、重试和切换符合已接受的重复执行风险边界，收存失败不重跑生成。
- 管理页仅包含本期配置功能；中英文、主题及窄屏行为遵循现有设计规范。

真实 Codex 或 Antigravity 生成验证会调用外部服务并消耗额度，必须在执行前取得针对测试账号与影响的明确授权；不能把本次设计讨论当作生产服务调用授权。
