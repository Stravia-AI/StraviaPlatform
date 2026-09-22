# 媒体生成

本文记录设计访谈中已确认的功能契约，不表示功能已经实现；实施须在整体设计确认后开始。架构取舍见 [ADR-0061](../adr/0061-bind-media-generation-to-routes.md)，领域术语见 [CONTEXT.md](../../CONTEXT.md)。

## 1. 范围与入口

Media Generation 的用户可见名称为“媒体生成”，属于 Advanced Capability。统一工具名为 `generate`；本期只支持图片生成与参考图编辑，首个接入后端为 Codex，不提前公布音频或视频类型。

工具通过平台执行，支持显式调用、MCP 及可选透明注入。媒体生成使用独立平台总开关，关闭时所有入口均不可执行。总开关开启后，所有有效 API Key 可显式调用；MCP 另受既有 MCP 开关约束。API Key 增加“媒体生成”透明注入选项，新建及已有 Key 均默认不勾选；该选项只控制自动暴露，不是能力授权。沿用 [ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md)。

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
  media: {
    width: number; // 实际像素宽度
    height: number; // 实际像素高度
  };
};
```

`media.width` 与 `media.height` 从实际生成文件读取，不以请求参数代填。结果不重复返回裸 Artifact ID，也不重复返回推算比例或分辨率档位。其他媒体类型的 `media` 字段随对应能力定义。

下载使用既有 `StraviaRead({path: "stravia://artifacts/<artifact-id>?download=1"})` 获取限时下载授权，不将下载地址当作稳定身份。保留期、归属、上传与下载授权沿用 [ADR-0048](../adr/0048-separate-artifact-references-from-transfer-grants.md)、[ADR-0049](../adr/0049-snapshot-explicit-media-inputs-as-artifacts.md) 和 [ADR-0051](../adr/0051-disambiguate-artifact-download-and-understanding.md)。

## 4. Route 与 Codex 接入

管理员按生成类型分别绑定 Route，当前仅有图片绑定。调用方不能覆盖该绑定。所有已启用 Target 必须满足当前类型的媒体生成适配器接入要求；绑定时与执行前均校验，不从混合 Route 中静默筛选候选。已禁用 Target 不参与资格校验。这里的专用约束不新增 Route 类型，也不禁止同一 Route 被普通推理引用。

按照已确认的 [Vendor 插件设计](vendor-plugins.md)，媒体生成可以与推理、完整搜索由同一 Vendor Plugin 和 Provider 连接提供，也可以是插件唯一的能力。沿用 Provider Model → Target → Route；Provider Model 可表示纯图片模型，不以聊天能力为前提，普通聊天执行不得选择不具备其所需能力的 Target。

插件拥有供应商参数映射、请求及结果解析；宿主继续拥有公开 generate 工具、权限、输入处理、Artifact 收存与交付，不允许插件自行注册 MCP 工具。能力移除更新保留绑定并明确不可用；内置自动更新不为此暂停确认，但数据丢弃仍须确认。兼容在途生成使用旧版本完成，不兼容更新取消生成并阻止迟到结果提交。

Codex 参考 [OMP image-gen.ts](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/tools/image-gen.ts) 的实现方式：

1. 通过 Route 选择 Target，复用相应 Codex Provider 的账号、认证和连接设置。
2. Target 上游模型是支持托管图片工具的 GPT 对话模型。
3. 调用 Responses，以 `tool_choice` 强制选择 `image_generation`；参考图决定生成或编辑操作。
4. 消费流式响应，提取图片，收存并返回 Artifact 引用及实际规格。

不运行 Codex CLI，不采用独立的 `images/generations` 或 `images/edits` 调用链，不切换为另行配置的 OpenAI API Key。实际支持的模型、参数映射及错误分类须以选定链路的源码与协议证据核定；目录声明不能代替接入资格验证，保存配置也不代表已经执行过真实生图验证。

## 5. 失败与重试

沿用普通推理的可重试错误分类与 Route 重试、Target 切换策略，参见 [ADR-0034](../adr/0034-layer-route-target-selection.md)。不因请求已发送且执行状态不明而额外阻止原策略允许的重试；明确接受重复生成、重复消耗额度的风险，不承诺恰好执行一次。

- 不将参数校验、参考图读取或收存失败归类为可重试上游错误。
- 不将用户取消重新归类为重试条件；已有输出提交边界继续适用。
- 上游没有有效图片、输出图片无效或 Artifact 收存失败，均不能报告成功。
- 输出图片已生成但本地收存失败时，不以恢复文件保存为由重新调用生成。
- 使用现有工具错误通道，不返回 `success: true` 的空图结果，不把失败伪装为异步任务成功。

## 6. 管理面范围

遵循根 [DESIGN.md](../../DESIGN.md)，在“高级功能”中增加“媒体生成”配置页。本期仅提供能力总开关、图片生成 Route 选择、配置校验与当前支持类型的说明，不提供手动试生成、参考图上传、图片预览或图库。

实际生成通过客户端工具调用、透明注入或 MCP 执行，不以管理员身份另建生成及 Artifact 所有权路径。配置不足必须说明缺失条件与补齐入口；配置保存、功能启用与真实请求成功是不同事实，不自动发送可能收费的上游测试请求。

## 7. 验收范围

实施时须通过产品入口验证以下已确认契约；本节是验收要求，不是已执行的测试报告：

- 统一输入严格按类型校验，图片生成与参考图编辑均可经 Codex 托管工具完成。
- 图片 Route 的绑定、运行前资格校验与普通 Route 选择行为一致；不静默过滤不兼容已启用 Target。
- 平台 Gate、显式调用、MCP 与透明注入组合符合既有高级能力规则，已有 Key 不自动启用媒体生成注入。
- 文件归属、收存、保留期及签名下载沿用 Artifact 契约；无跨 Principal 引用和本地路径泄漏。
- 实际图片像素与返回的 `media` 一致；近似映射不伪报原请求尺寸。
- 错误分类、重试和切换符合已接受的重复执行风险边界，收存失败不重跑生成。
- 管理页仅包含本期配置功能；中英文、主题及窄屏行为遵循现有设计规范。

真实 Codex 生成验证会调用外部服务并消耗额度，必须在执行前取得针对测试账号与影响的明确授权；不能把本次设计讨论当作生产服务调用授权。
