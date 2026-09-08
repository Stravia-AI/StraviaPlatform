---
status: accepted
---

# Vendor 按请求用途拥有推理与模型探测的构造契约

本 ADR 的请求构造迁移已落地：Vendor、VendorExtension、推理 pipeline 与管理模型探测统一使用 `construct_request`，旧的认证和 URL 钩子已删除。

迁移前，管理面的 Provider 模型查询与 Route 模型发现分别拼装认证、URL 和 runtime binding，另有一套按协议字符串判断的认证规则。推理侧 Vendor 的分离认证与 URL interface 没有明确区分请求用途，不能把推理的模型路径改写和认证方式直接用于任意模型列表端点。

选择演进现有 Vendor / VendorExtension interface，显式区分推理与模型探测两个实际用途，由同一个 Vendor module 拥有各自的请求构造约定。这样删除的是平行维护的认证知识，而不是只把两个管理 helper 搬到一起；代价是一次公开 Rust interface 的干净迁移。

## 接受的 interface 形状

- 推理用途携带实际 egress 协议、base URL、codec 产出的相对路径与实际模型；模型探测用途携带调用方已选定的完整端点，不携带假的模型名。
- Vendor 接收已解析的凭据上下文与用途，产出最终 URL 和 headers。它不发送请求、不解析模型列表，也不决定 catalog、静态列表、override 等来源的选择顺序。
- 默认认证抑制必须同时约束 header 与 URL 中的默认凭据；不能只禁止 `x-api-key`，却继续追加默认 query key。runtime binding 明确提供的 headers 保持原有覆盖优先级。
- 协议别名只由既有 ProtocolRegistry 解析，不在管理调用方或 Vendor 用途分派之外维护局部别名表。URL 凭据参数使用结构化编码，不直接拼接凭据字符串。
- 请求发送方按 `Provider.use_proxy` 和现有全局代理规则选择 client，不另造代理策略。两个探测入口的超时与错误呈现继续由各自 owner 拥有。

具体 Rust 类型名称不是本 ADR 的约束；用途显式、知识归属与删除旧路径才是约束。

## 模型端点与认证

模型探测端点不必与推理端点使用相同协议。例如，当前 Google 预设的模型列表地址是 `/v1beta/openai/models`，不能因为 Provider 的推理协议是 Gemini，就机械套用原生 Gemini 请求构造。

Google 自有原生 `models.list` 端点及从保存 base URL 自动选择的原生列表继续使用编码后的 API Key query；自定义代理端点不能仅因路径看似原生就改变 Models Bearer 约定。默认认证抑制同样覆盖这条 query 分支。

仅使用 API Key 的普通连接允许空凭据，包括自定义端点及目录中的 OptionalApiKey 通道。运行时将空密钥解析为 `disable_default_auth`，模型发现与推理沿用同一默认认证抑制机制，不产生空 Bearer 或默认 query key。填写密钥时保留原认证行为；OAuth、Setup Token、Vertex 与包含额外凭据字段的 Adapter 仍执行原有凭据校验，不能把缺失必需凭据解释为无认证请求。该规则不改变客户端访问 Stravia 的认证要求。

已知内置端点和 OAuth binding 使用其明确约定。没有额外端点语义的自定义 `models_source` 继承该 Vendor 明确声明的模型探测认证约定；不根据任意 URL 猜测，不自动轮试多种认证，不新增持久化认证配置字段。自定义端点不符合该约定时，沿用所属入口的既有查询回退或同步报错语义。

## 明确保留的职责

[ADR-0026](0026-own-provider-write-and-route-bind-as-two-modules.md) 的 Provider 与 Route 分工不变：

- 查询 Provider 模型列表仍可在现有条件下回退静态列表。
- Route 的 Provider Model 同步仍对发现失败明确报错，不把失败解释为空列表或同步成功。
- 来源优先级、OAuth runtime 解析、模型列表解析和 Provider Model 对账不进入请求构造 module。
- 不改变 wire codec、管理 HTTP 字段或持久化 schema，不借此增加上游重试或新的模型发现能力。

## 不采用的方案

- **只提级管理侧 helper：**只能消除两个探测调用点之间的重复，仍与推理侧平行维护认证，depth 收益不足。
- **要求调用方提供完整端点认证语义：**interface 看似更通用，却把模型端点分类知识继续暴露给调用方；当前任意 override URL 也没有足够事实支持这种要求。
- **额外探测 façade：**如果只是转发共享构造，没有新增 locality 或 leverage；如果同时接管来源、发送与回退，则越过本次范围。
- **统一探测与同步的失败语义：**这是另一项用户可见行为决定，不随本次构造深化改变。

## 实现与验证边界

Vendor、VendorExtension、现有 Vendor/Channel/ProtocolDefault adapter、生成实现的宏及转发实现均使用 `RequestPurpose` 区分推理与 Models，返回 `ConstructedRequest`。管理侧已删除独立认证 helper 和 Route 内联构造；推理侧使用同一构造契约，不保留兼容 shim。既有需要异步 token 获取或签名的推理 orchestration 仍由对应 Vendor 拥有，不为 Models 新增发现来源或 token 交换能力。

通过同一 interface 观察最终请求，覆盖规范协议名与别名、内置端点和自定义端点、禁用默认认证、binding headers 覆盖、凭据保留字符编码及代理出口。用本地 HTTP adapter 验证实际请求，不访问生产上游，不把凭据写进测试失败日志。

保留模型查询回退、Route 发现错误、OAuth 默认认证抑制及现有推理请求行为测试。公开 Vendor interface 迁移必须核对所有调用方；构造统一不能以破坏非通用 Vendor 的路径、签名或认证为代价。

现有 Admin HTTP 的 `/test-models` 与 `/models/sync` 均进入 Route 发现；带静态回退的 `AdminService::get_provider_models` 没有 HTTP route，因此其回退契约通过已有 AdminService interface 与本地上游验证，不为测试增加 wire endpoint。
