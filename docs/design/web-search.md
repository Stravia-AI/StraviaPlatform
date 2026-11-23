# Web Search 与 Web Fetch

Web Search 执行一次完整研究并返回带来源的报告；Web Fetch 读取具体公网 URL，不启动研究会话。
两者经同一个 [StraviaRead](agent-core.md#straviaread-统一读取契约) 入口，共用 Web Search 平台 Gate 和 `web_search` 注入项；
显式调用与 MCP 的权限见[平台能力门禁与透明注入](agent-core.md#平台能力门禁与透明注入)。

[stravia-web-search](../../backend/crates/stravia-web-search/) 拥有研究执行、Local/External backend、配置策略和报告校验。
Core 通过宿主接口提供 Agent、权限与来源快照，并装配 MCP 和管理入口。
基础来源及网络执行归 [Web Access Provider](web-access-providers.md)，不用完整研究接口替代基础检索。

## 搜索输入与报告

`path: "stravia://search"` 开始新研究；`body.question` 是非空白搜索文本，最多 64 KiB。
可用 `body.allowed_domains` 约束报告来源：原始列表最多 20 个非空条目，域名规范化并去重；
空列表与省略等价。该列表不授予网络访问权限，也不支持 `blocked_domains`。

成功结果包含 Turn `path`、`completion` 及 `report`。示例中的 ID 是占位符：

```json
{
  "path": "stravia://turns/<id>",
  "completion": "complete",
  "report": {
    "answer": "已核对的结论 [stravia://turns/<id>/sources/1]",
    "sources": [
      {"path": "stravia://turns/<id>/sources/1", "url": "https://example.com/source", "title": "来源标题"}
    ],
    "limitations": []
  }
}
```

- `SearchReport` 对答案、来源数量和限制说明设上限。`partial` 必须注明预算、时限等原因；不把不完整的研究报为完成。
- `Search Source` 使用当前 Turn Path 加 `/sources/<ordinal>`（十进制编号）；
  答案中的方括号引用与来源列表一一对应。
- 来源必须是规范化的公网 HTTP(S) URL，且通过 DNS 和已解析的允许域名约束。
  域名越界报 `source_outside_allowed_domains`，不删来源后发布貌似完整的报告。
- Local 仅采用当前或祖先 Turn 的已验证工具证据；标题取有界证据标题，缺失或过长可省略。
  External 采用供应商来源与标题，不按 Local 证据匹配，但同样校验报告和公网地址。

完整报告先校验并提交；长答案交付复用 [Read Snapshot](agent-core.md#straviaread-统一读取契约)。
来源列表不会为了答案分页而被裁剪。

## Search Turn 与续接

Search Turn 是 Principal 内不可变的研究结果，存储类型是 `web_search`。
成功报告的 `stravia://turns/<id>` 为显式引用；没有隐式 latest。

| 场景 | 行为 |
| --- | --- |
| Local 新根 | 固定 backend、模型绑定、配置 Revision 和研究预算。 |
| Local 续接 | 以父 Turn 引用作为 path，附后续问题。恢复指定父链，可从任一可读且兼容的父节点分支。 |
| Local 域名策略 | 省略或传空数组继承父策略；非空列表替换。要清除继承限制，应新建根。 |
| External 结果 | 仍有报告与来源引用，但没有跨请求研究会话。对其发起续接会明确失败。 |

不兼容的 Definition Revision 明确要求重新开始，不改写历史、不静默重开根。
续接授权与保留规则见 [Turn Chain](agent-core.md#turn-chain-与-turn-reference)。

## 搜索后端

一次搜索只执行所配置的 backend。两者互不回退；External Route 内的 Target 切换与 backend 切换不是一回事。

### Local Agent

Local backend 使用内部 Agent Definition，调用基础搜索与抓取，在预算内收集、核对证据并生成报告。

- 需要已保存、启用的对话推理模型，且实际执行 Target 支持工具调用；
  搜索和读取各需要至少一个可用来源。
- `backend.model_id` 使用模型的内部记录键 `models.id`，不按公开名称或供应商 selector 回退。
  公开改名不影响绑定，停用或删除后不得执行。
- `max_turns` 限制研究轮数；时限取 `total_time_seconds` 和调用方 deadline 的较早值。
- 内部 Agent 的同名 Read 只执行基础检索，不再发起一层完整研究，见[读取运行层级](agent-core.md#straviaread-统一读取契约)。

### External Route

绑定用途为 `web_search` 的 Route；`backend.route_id` 记录 Route 公开名称 `models.model_id`。
合格 Target 必须选择具备完整搜索能力的 [Vendor Plugin](vendor-plugins.md) 渠道与真实、非空的上游模型。
该用途不凭文本或图片输出模态判断，也不进入普通推理目录。

- 插件复用所选 Provider 的认证和连接，返回归一答案、引用与实际已知用量。
  基础链接列表不是完整报告；缺失用量不能伪造为零。
- 调用受请求取消、deadline、响应大小与终态校验约束，不执行 Local 研究预算。结果提交前可按 Route 策略对明确可重试的上游错误重试或切换 Target；
  取消、参数及权限错误不触发重试，提交后不重放。重试可能重复消费额度。
- Google grounding 的特定跳转来源在报告前通过 Web Access 解析为最终地址。同一结果中的重复地址复用解析；
  先检查数量、长度，沿用成功 Target 的代理快照、取消与时限。
  解析失败不发布原跳转地址，也不丢弃该来源后假报成功。
- 外部搜索不续接上游会话，即使上游支持续接标识；Web Fetch 独立选择自己的来源，不借用完整搜索 Route 抓页面。

## Web Fetch

Web Fetch 读取公开 HTTP(S) 资源。HTML 通常提取正文并转为 Markdown；JSON、XML 与纯文本读文本。
HTML 附问题不会替用户回答，返回 `question_applied=false`；
图片或 Office 问答由[多模态理解](media-understanding.md)处理。

`:raw` 获取一次正文并严格解码，依次采用 BOM、Content-Type charset、HTML meta、默认 UTF-8；
未知或损坏编码失败。默认阅读转换可有损，raw 正文不插入行号或提示。
未知二进制明确返回未读取信息，不能用下载冒充读取；下载不启动模型。

响应受来源、批量公平预算与超时限制。内部研究网页请求允许 500,000 字符，其他读取缺省为 8,000 字符。
显式较大预算可以提高批量总量，但不能超过供应商限制。

例如 Exa 的 [Get Contents](https://exa.ai/docs/reference/get-contents) 结果保留 10,000 字符上限。
触顶时如实报告源截断，不把源截断伪造成还有续页。

## 安全与管理

- [Web Access 公网规则](web-access-providers.md#公网读取安全)适用于实际读取和重定向；
  静态筛查不能代替执行时检查。Local 证据必须由真实工具调用获得，上传快照的来源注记不能授予证据身份。
- 网页及供应商输出不可信。普通进度事件只暴露标识、阶段、序号；
  错误不返回原始报文、凭据、请求头或堆栈。
- Admin 使用 `GET/PUT /api/v1/web-search/config`、`GET /api/v1/web-search/eligible-models` 与 `GET /api/v1/web-search/external-routes`；
  `web_search_config` 拥有 Revision、启停、backend 绑定与 Local 预算。
- 未绑定时配置页默认呈现 External Route，不自动选择、绑定或启用。仅已保存的 Local 绑定显示来源和预算；
  来源优先级独立保存，模型和预算显式保存，启停不会提交草稿。
- 已绑定 Search Route 即使停用也须先解绑才能改用途。External 不要求 Web Provider；
  切回 Local 时重新校验保留的 Local 预算与来源。
