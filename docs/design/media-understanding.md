# Media Understanding 设计

> 状态：已实施
> 更新：2026-09-10
> 相关决策：[ADR-0009](../adr/0009-add-media-understanding-as-capability-tool.md)、[ADR-0016](../adr/0016-gate-advanced-capabilities-and-separate-transparent-injection.md)

## 1. 结论

实现归属独立 `stravia-media` crate，包含 Definition、配置策略、媒体预处理、Derivative 存储、报告校验、ingest/snapshot、bridge 规划及 Platform Tool 行为。`stravia-core` 在编译期注入 `MediaHost`、`MediaAgentHost`、`MediaArtifactHost`，并保留 MCP/管理面 Adapter；能力不依赖 Gateway 类型。共享 IR、Agent、Artifact、Hook 与身份契约来自 `stravia-runtime-contract`，不复制类型或通过 JSON 往返 canonical 数据。

Media Understanding 是由独立平台开关控制的 Advanced Capability，通过 `StraviaRead` 的 Artifact URL `question` 参数调用；普通模型请求和 MCP 共用同一个 Media Report contract。统一入口的授权、收存、下载和路由归 core，媒体理解仍由本能力执行。参见 [ADR-0048](../adr/0048-separate-artifact-references-from-transfer-grants.md) 至 [ADR-0051](../adr/0051-disambiguate-artifact-download-and-understanding.md)。

用户可见名称采用“多模态理解”，为未来 PDF、视频和音频扩展保留产品语义。本 Revision 的运行时仍只支持静态 JPEG、PNG 与 WebP 图片；页面不展示或承诺未来格式。

平台 Gate 开启后，每个有效 API Key 都能通过统一入口显式请求理解。关闭后，该分流不可调用，但裸 Artifact 下载不受媒体开关影响。API Key 的 Transparent Injection 只选择本次自动暴露的能力，不承担显式授权；执行层仍检查本次暴露范围。

## 2. 公开 contract

工具 wire name：

```text
StraviaRead
```

输入：

```json
{
  "url": "https://stravia/artifact/artifact_...?question=Describe%20the%20image",
  "previous_turn_id": "agt_..."
}
```

`question` 只解释于 Artifact Reference。裸引用返回下载信息；外部图片 URL 先收存，再执行描述和文字提取，其原始 query 完整保留。`previous_turn_id` 保留同 Principal 的续接与分支，已经在祖先中保留的 source 复用，不重复附加；同次调用的重复 source 仍拒绝。旧平台调用别名不注册，旧持久化记录不批量改写。

结果：

```json
{
  "turn_id": "agt_...",
  "completion": "complete",
  "report": {
    "answer": "The image contains ... [artifact:artifact_...]",
    "artifacts": [{ "artifact_id": "artifact_..." }],
    "limitations": []
  }
}
```

`MediaOutputValidator` 保证：

- answer、artifacts 和 limitations 满足大小与数量边界；
- answer 中每个 marker 对应一个实际列出的 source Artifact；
- 每个 Artifact 属于当前 principal，并来自当前或祖先 Media Turn；
- derivative ID、用户伪造 ID 和其他 principal 的 Artifact 不能进入报告；
- partial 结果说明预算、deadline 或覆盖范围限制。

## 3. Gate、显式调用与 MCP

| 调用面 | 条件 | Transparent Injection 的作用 |
|---|---|---|
| 显式 `StraviaRead` 媒体分流 | Media Gate 开启且 Key 有效 | 无 |
| MCP 媒体调用 | Media Gate、有效 Key、`mcp_access_enabled` | 无 |
| non-vision parent 自动 bridge | Media Gate、有效 Key、master 与 `inject_media_understanding` | 决定是否启用 bridge |
| 原生 vision Target | 逻辑 Model 自身的图片能力 | 无；原生路径优先 |

Gate 关闭时，API Key 已保存的 `inject_media_understanding` 保留但运行时忽略。重新开启 Gate 后，该选择恢复生效。

## 4. 路由策略

含图片的普通推理请求按以下顺序规划：

1. 逻辑 Model 存在 eligible native vision Target：固定使用 native 路径，已收存 Artifact 在实际调用前生成对应传输表示；
2. 没有 native Target，但存在 tool-capable parent Target，且 Media Gate 与 Transparent Injection 均允许：使用 bridge；
3. 其他情况：在上游调用前返回明确的 input/capability error。

native 路径永远优先，不因为 Media Tool 可用而改写为 bridge。一个 Inference Run 选定路径后不在失败时自动切换语义。

bridge 会：

- 复用输入收存得到的 principal-scoped source Artifact；原始输入只允许明确的结构化内联媒体或公网 HTTP(S) 附件；
- 在原始图文位置写入稳定 Artifact marker；
- 从发送给 non-vision parent 的内容中移除原始图片 bytes/URL；
- 注入 code-owned 安全说明，并把媒体能力并入 `StraviaRead`；
- 让父模型使用 marker 中的稳定 Artifact Reference 与 URL 编码的问题。

## 5. 内部执行

Media Understanding 使用 `id = "media-understanding"` 的 internal Agent Definition。它不出现在 Agent Admin list，也不生成通用 `agent_*` surface。

执行复用：

- `AgentRunner` 的 model execution、repair、cancellation、usage 和 Turn persistence；
- `ArtifactStore` 的 principal ownership、TTL 和 immutable bytes；
- `MediaInputPreprocessor` 的格式验证与 JPEG derivative；
- `MediaOutputValidator` 的 Artifact provenance；
- `TurnChainStore` 的 continuation 和 branch。

管理员配置的逻辑 Model 必须启用，且每个 Target 都必须支持图片输入。管理员还必须从该逻辑 Model 的 `supported_thinking_levels` 中选择思考等级；每次内部 Model Turn 都携带该等级。隐藏 Media Model 不需要出现在 API Key 的普通 `model_ids` 中；平台 Gate 开启后，有效 Key 通过 capability-owned authorization 间接执行它，但不能把该隐藏 Model 当普通客户端 Model 直接调用。

## 6. 当前图片处理边界

本 Revision 接受：

- `image/jpeg`；
- `image/png`；
- static `image/webp`。

运行时依据实际 container 解码，不只信 MIME。GIF、animated WebP、HEIC/HEIF、SVG、PDF、视频和音频会返回明确的不支持错误。

每个 source Artifact 首次使用时生成 write-once JPEG derivative：应用 orientation、白底合成 alpha、限制尺寸、移除 metadata，并把 mapping 持久化。公开 contract 和 Media Report 始终引用 source Artifact，不暴露 derivative ID。

这些限制属于运行时错误 contract，不属于管理员配置项。未来增加新媒体类型时必须使用新的 Definition Revision，并保持旧 Turn 的 Revision 语义。

## 7. 安全边界

- 图片和其中的文字都是不可信数据，不能改变 system instructions、authorization、Artifact allowlist 或 tool policy；
- HTTP(S) ingest 在初始 URL 和每次 redirect 上执行公网地址、DNS、实际连接、字节数和 deadline 检查；
- ArtifactStore 再次验证 principal owner；
- 统一读取先验证当前 Principal 的 Artifact 归属，同 Principal 可跨对话复用，不能用公开下载入口绕过归属；
- MCP 可使用调用 principal 自己的 ready Artifact；
- ordinary logs 不记录图片 bytes、下载 URL、tool arguments/results 或 derivative mapping；
- capability 在运行中被撤销后，下一次隐藏 side effect 前终止，不提交新的 Media Turn。

## 8. Admin surface

Core Admin 与 Server/Desktop 共用：

```text
GET /api/v1/media-understanding
PUT /api/v1/media-understanding
```

WebUI route：

```text
/media-understanding
```

页面只显示：

- enabled；
- logical Model；
- Thinking Level；
- effective state：`disabled` / `unavailable` / `available`；
- Save。

页面不展示 OCR/描述/比较等虚假子能力，也不展示格式、文件大小、像素、JPEG profile 或 derivative 实现信息。

## 9. Persistence 与升级

Media Definition、Agent Turn、Artifact 与 `media_derivatives` 继续使用既有 schema。migration 26 为 `agent_definition_configs` 增加 nullable `thinking_level`；启用 Media Understanding 时，专用 Admin API 要求该值属于所选逻辑 Model 的支持等级。migration 18 把 API Key 权限改为：

- `mcp_access_enabled`；
- `transparent_injection_enabled`；
- `inject_media_understanding`；
- `inject_web_search`。

旧 `allow_media_understanding` 只在 migration 中用于恢复既有自动工具行为，然后删除；它不再是显式 capability grant。

这是权限扩大的 clean cutover：Media Gate 开启后，所有有效 Key 都能显式使用能力。升级前必须备份数据库和匹配的旧二进制。回滚必须恢复 migration 18 之前的数据库；不能只回退应用文件。

## 10. 验证边界

- Gate 开／关对 `StraviaRead` 媒体分流和 MCP 调用的影响，裸 Artifact 下载独立可用；
- Transparent Injection 关闭时显式 MCP 仍可用；
- Gate 关闭时保存的注入选择保留但不生效；
- native vision Target 优先于 bridge；
- 只有每个 Target 都支持图片输入的已启用逻辑 Model 才会出现在配置列表；
- 配置的思考等级必须由逻辑 Model 的每个 Target 支持，并应用到每次内部 Model Turn；
- 无 native Target 时 bridge 仍可执行完整 Media Report；
- JPEG/PNG/WebP 成功，不支持的未来格式返回明确错误；
- WebUI 只显示核心配置和 effective state。
