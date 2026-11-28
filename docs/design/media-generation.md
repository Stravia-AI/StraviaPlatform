# 媒体生成

媒体生成目前只支持文本生图与参考图编辑。公开工具的 wire name 是 `StraviaGenerate`；两种操作使用同一个工具和图片 Route。
显式调用、MCP 与自动暴露见[平台能力门禁与透明注入](agent-core.md#平台能力门禁与透明注入)；
有界等待及后台状态查询见[工具执行交接](agent-core.md#工具执行交接)。

## 工具输入

```json
{
  "type": "image",
  "input": {
    "prompt": "保留参考图中的小屋，把背景改为雪山",
    "aspect_ratio": "16:9",
    "resolution": "2K",
    "reference_images": ["stravia://artifacts/<artifact-id>"]
  }
}
```

| 字段 | 规则 |
| --- | --- |
| `type` | 必填，目前只接受 `image`。 |
| `input.prompt` | 必填非空白。 |
| `input.aspect_ratio` | 可选，`1:1`、`3:4`、`4:3`、`9:16`、`16:9`。 |
| `input.resolution` | 可选，`1K`、`2K`、`4K`。 |
| `input.reference_images` | 最多 5 项；顺序对应「图 1、图 2……」。省略或空数组表示文生图。 |

拒绝未知字段；不接受调用方覆盖 model、provider、route，也不接受 count、action 或历史图片索引。
尺寸档位只是偏好，映射由所选适配器决定，可能近似或使用供应商默认值。
每次成功只返回一张图片，不能静默截断参考图或忽略它们改做文生图。

## 图片输入与产物

接受不带选择器的所属 Principal Artifact 引用或公网 HTTP(S) 图片 URL。参考图只支持 JPEG、PNG、WebP，每张最多 32 MiB、25 百万像素，单边不超过 8,192 像素；供应商可有更严限制。
不接受客户端路径或 inline base64；本地文件走 [Artifact 上传](agent-core.md#artifact-与单请求上传)。
公网输入先安全收存并校验，不透传原始 URL；上游不接收 `stravia://` 引用。

成功前验证图片并完整收存到当前 Principal。结果包含：

- `path`：稳定、可复用的 `stravia://artifacts/<id>`。
- `mime_type` 与 `size`：实际格式和字节数。
- `media.width` 与 `media.height`：解码所得实际像素，不以请求参数代填。
- `download_url`：15 分钟 bearer 下载链接，不是文件身份，不另返回 `expires_at`。

工具结果的链接始终从已配置完整 `client_base_url` 通过平台读取，即使启用 S3 直连也不改用外部地址。
生成前校验该入口。展示用 `![图片描述](download_url)`，不向用户或任何客户端输出 `stravia://` 内部引用（包括正文、Markdown 链接、title 或 alt）；
`path` 仅留在工具参数/历史恢复中供参考图复用或通过 `StraviaRead`（`<path>:download`）取得新链接，不重新生成。后台查询原样返回链接，不自动续签。

产物已保存但链接签发失败时，返回 `artifact_download_failed` 并携带 `artifact` 的路径、MIME、大小和像素规格；
保留产物，不重跑上游。文件和签名授权的生命周期见 [Artifact 契约](agent-core.md#artifact-与单请求上传)。

## Route 与 Provider 接入

管理员在「高级功能 → 媒体生成」绑定一个已保存、启用且有可执行 Target 的 `image_generation` Route。
Route 的 `purpose` 是单一用途，不和 `inference`（对话推理）或 `web_search`（联网搜索）混用。
媒体配置引用存在时，即使关闭总开关也不能改为其他用途；需明确解绑，停用或删除 Target 不等于解绑。

- 图片输出资格按 Provider Model 的原始登记判定。缺失、空输出集合、仅有图片输入都不等于图片输出；
  强制可用不能绕过资格。绑定、候选查询和执行前均校验。
- 一个图片 Route 可混合文本输入与支持参考图的模型。带参考图时还要求实际模型明确登记图片输入；
  资格在重新选路与发送前复验，不依据旧附件标志。
- 输出模态漂移时保留 Target 及绑定，排除失配项，兼容目标仍可执行。
  适配器能力与输出登记分开校验：其他适配器不兼容不能静默跳过，禁用项不参与运行态适配器检查。
- 没有可执行目标时在上游发送前失败。`media_generation_targets_purpose_mismatch` 指向登记或 Target 失配，不是启用状态错误。
- 合格 Target 使用同一套优先级、均衡/延迟偏好、冷却、重试和切换规则。原生、托管、文生图与图生图不各排优先级；
  独立生成不建立 Continuation 或 Affinity。

管理创建 Route 时显式提交用途，表单默认对话推理。仅一键接入可在创建时按推理、图片、搜索顺序推断首个合格用途；
显式绑定只向既有 Route 添加 Target，缺失返回 `ROUTE_NOT_FOUND`。切换用途前须校验所有保留的 Target，包括禁用项；
可同次替换失配项，不自动删除绑定。

**既有 Route 的用途。** SQLite 与 PostgreSQL 的 `0022_default_existing_routes_to_text.sql` 仅把未配置输出的历史 Route 回填为文字，保留明确选择的文字或图片；
`0057_route_purpose.sql` 分别映射为 `inference` 与 `image_generation`。原 Route、Target、启用状态、调度设置及图片绑定保留；
外部搜索使用独立复制的 `web_search` Route，不改写原目标用途。原先不具备输出资格的搜索目标在副本中保持不可调度。
若旧图片引用指向了现在的对话 Route，需修复用途与失配目标、改绑或解绑；这不使历史对话 Route 本身不可执行。
通用的备份与耐久规则见[升级迁移原则](data-layout-performance.md#升级迁移原则与约束)。

[Vendor Plugin](vendor-plugins.md) 拥有发现、上游参数与结果解析，宿主拥有工具、权限、输入处理和产物。
同步只更新未固定的规格；目录能力、当前账号授权与实际生成成功是三件事。
更新后失去能力的绑定保留并显示不可用，不自动改绑。

### Codex OAuth

复用 Route 所选 Codex Provider 账号和连接，以 Responses 的 `tool_choice` 强制 `image_generation`，从完成的流式结果提取单图；
不调用 Codex CLI，也不切换到独立 Images API 或其他 API Key。

同步只用明确的 `media_image`、图片输出声明或 `experimental_supported_tools` 中的 `image_generation` 判断能力，不猜型号前缀和版本。
`input_modalities` 与 `supports_image_detail_original` 只说明输入。保留真实文字输出；
不会把图片专用或显式空输出的条目补成聊天模型。

### Antigravity

从账号 `fetchAvailableModels.imageGenerationModelIds` 发现能力，解析已声明的废弃 ID 重定向并过滤禁用项。
使用真实模型 selector，不按推理档位折叠、猜测图片输入或取广告列表首项。相同 ID 的真实文字能力保留。

复用 OAuth、project 和连接。`POST /v1internal:generateContent` 使用定长 JSON，`requestType: image_gen`，提示词在有序 `inlineData` 参考图之前，`candidateCount: 1`；
缺省 `aspectRatio: "1:1"`，显式档位映射为 `imageSize`。不加 SSE、system 指令或 `responseModalities`。
响应必须以 `STOP` 完成且恰有一张有效 `inlineData` 图片，解码上限 32 MiB。

### MiniMax

内置 `base` 的普通国际/中国区和 Coding Plan Profile 共用此适配器。同步读取官方[文生图](https://platform.minimax.io/docs/api-reference/image/generation/api/h3-image-generations.json)和[参考图编辑](https://platform.minimax.io/docs/api-reference/image/generation/api/h3-image-edits.json) OpenAPI（中国区使用对应文档域），仅沿已实现 request schema 读取 `model.enum`。
编辑枚举只为已发现同型号增加图片输入。两份 schema 都必须成功；
失败保留上次库存，不伪报同步成功。

按参考图有无选择 `/v1/images/generations` 或 `/v1/images/edits`，固定单图非流式 base64 返回；
`aspect_ratio` 直接映射，分辨率映射到 `area`，参考图用有序 data URI。官方区服使用对应图片域，自定义基址保留部署前缀。
上游可等待最长 600 秒，仍受平台时限约束。

Plan 使用订阅额度和已有 Credits，按量 Key 使用账户余额。图片本身受周额度限制；
若开启五小时 Provider 级守护，耗尽仍会连带暂停同连接图片 Target，见[额度守护决策](../adr/0078-suspend-provider-routing-on-guarded-allowance-exhaustion.md)。

### 火山引擎 / BytePlus

内置 `base` 的标准 Ark 与 BytePlus 通过 `GET {base}/models` 的图片输出、接口或领域声明发现图片模型，以明确图片输入或 `image_to_image` 判定编辑资格。
Agent/Token Plan 尝试 `/api/plan/v3/models`，404 时保留目录已声明或人工登记的资格；
Coding Plan 不广告未开放的图片操作。

生成和编辑统一请求 `{normalized_base}/images/generations`，Plan 规范化到 `/api/plan/v3`，自定义前缀保留。
固定 `response_format: "b64_json"`；比例和档位近似映射为 `size`，参考图通过 `image` 传单字符串或数组（适配器单张 30 MiB、整包 64 MiB）。
32 MiB 响应上限；缺少 `output_format` 时按 PNG/JPEG/WebP 字节头识别，再由宿主解码校验。

### 阿里云百炼 / DashScope

内置 `base` 的标准国际/中国区和 Token Plan 支持同步图片接口，Coding Plan 不声明此能力。
目录 `GET {api_root}/api/v1/models` 根据 `IG`、Image 输出和 Text 输入发现候选，Image 输入登记编辑能力；
排除旧异步或已下线模型。Token Plan 缺少 Key 或 404 时保留目录已声明及人工登记的条目。

文生图和编辑共用 `/api/v1/services/aigc/multimodal-generation/generation`。参考图先于提示词，以 data URI 放入 `input.messages[0].content`；
适配器单张限 20 MiB、整包 64 MiB。尺寸按型号映射 `宽*高`；
Wan 2.7 仅指定分辨率时传档位，`wan2.7-image-pro` 的 4K 仅限文生图。

接受单张 Data URL 或符合插件白名单的临时 OSS 结果；后者用无凭据 GET，限制 32 MiB 并验证实际格式。
不会把任意返回 URL 当作可信下载地址。

## 失败、重试与额度

Route 只按既有可重试错误策略重试或切换。参数错误、参考图读取/收存失败和取消不归类为上游可重试错误；
产物收存或链接失败不为恢复本地写入重跑生成。响应没有恰好一张有效图片，或收存失败，均不能报告成功。

上游错误按内容审核、配额、限流、参数/模型等类型处理，明确拒绝不作为断流盲目重试。
Antigravity 只给明确政策 `finishMessage` 的拒绝也归类为 `ContentFiltered`，不切换或计入共享健康失败。
公开错误保留已脱敏的可解释原因，认证及 HTTP 401/403 只给安全摘要。

取消和超时只约束本地执行，不保证上游停止或不计费；重试及同样输入再次调用可重复生成、扣余额或消耗套餐额度，不保证恰好一次。只记录上游明确给出的使用量。

## 管理面

`GET/PUT /api/v1/media-generation/config` 读写启停和图片 Route，`GET /api/v1/media-generation/eligible-routes` 列出候选。

配置页提供总开关、图片 Route 选择/解绑及配置状态。选择有效 Route 即时保存；失败恢复服务器确认值并就地显示原因。
解绑需确认，同时关闭媒体生成；配置失效仍可关闭或解绑。

没有候选时展示缺失条件与模型管理入口；禁止启用的原因在启停控件处可查。
保存成功后刷新 Route 的媒体绑定状态，编辑器不能以旧快照判定占用，也不能因刷新覆盖草稿。

配置完成不发送测试请求，管理员没有独立的生成、上传、预览或图库入口。实测生成会消耗上游额度；
保存配置、能力启用和请求成功是不同事实。
