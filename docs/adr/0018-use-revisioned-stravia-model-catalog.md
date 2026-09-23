# Use the revisioned Stravia Model Catalog as a template source

Status: accepted

> [ADR-0073](0073-register-runtime-catalog-profiles-through-base-vendor.md) 部分修订供应商目录所有权、跨模块 revision 协调与 scoped 获取失败处理：base 运行时拥有 Provider Profile 及供应商模型目录，Core 保留 Canonical Model 数据用于受限元数据回退；两边独立刷新，不再跨模块原子切换。本 ADR 的其他缓存与模型快照契约继续有效。

Stravia 将模型目录事实源从 models.dev 干净切换到 `models.stravia.cn`，运行时不再下载完整 `/api.json`。按 [ADR-0073](0073-register-runtime-catalog-profiles-through-base-vendor.md)，base 消费 `/providers.json` 与按需 `/providers/{provider}/models.json`，Core 消费 `/models.json`；两边分别校验各自下载的一致性，可以短时处于不同 revision。这样保留现有 Provider 创建后自动同步与账号级 discovery seam，同时避免重复下载完整 Provider Model 目录。

Canonical Model 以 `{lab_id}/{model_id}` 标识，只作为创建逻辑 Model 与手动 Provider Model 的一次性模板，不形成持久绑定或自动 overlay。逻辑 Model 选择模板时使用完整 canonical ID 作为客户端模型名；手动 Provider Model 复制除 `id` 外的完整 canonical 记录，把末段 `model_id` 预填为仍可编辑的 upstream model ID。Provider-scoped 条目已由上游应用 Canonical Model 基础数据与 Provider 覆盖，Stravia 不再次合并；导入后继续由 ADR-0054 定义的可编辑 Provider Model 快照承接。

## Consequences

- 两个模板搜索框都按 Canonical Model 的名称与完整 ID 搜索，并继续允许目录外手工输入。
- Lab 只作为 Canonical Model ID 的命名空间；本次不缓存 Lab 数据、不代理 Lab logo，也不新增 Lab 浏览或展示 UI。
- Provider Model 模板由 Core 按 canonical ID 从当前 revision 复制；不固定用户选择时看到的旧 revision。没有模板时生成 bare metadata：upstream model ID 加保守占位规格（text 输入/输出、256K 上下文、推理与工具调用），仍视为未登记、可由后续同步补模板；不再 fuzzy 猜测 Canonical Model。
- base 的供应商目录与 Core 的全局 Canonical Model 目录分别校验并切换各自数据，不再要求同一 revision 的跨模块原子切换，也不互相等待刷新；接受两边短时 revision 不同。任一侧失败都不得标记成功，并继续保留已有有效数据。
- 按 [ADR-0073](0073-register-runtime-catalog-profiles-through-base-vendor.md)，供应商模型目录获取失败必须明确报告；当模型集合已由上游发现或管理员添加确定时，允许只用 Core Canonical Model 补充缺失元数据且不得扩大模型集合。真正的上游账号级模型发现失败不适用该回退。插件化迁移前原生实现会把供应商缺项报告为 `ProviderNotFound`，而不是空 scope 成功；本条只在模型集合已被独立确定时新增元数据回退，不改变该失败事实，也不恢复已删除 Catalog 条目的新建资格。除此处明确修订外，base 的目录行为及元数据匹配顺序遵循 [ADR-0073 定义的插件化迁移前原生基线](0073-register-runtime-catalog-profiles-through-base-vendor.md#迁移基线)，不得从插件化后的当前实现反推。
- Core 的 Canonical Model 离线 bootstrap 保持既有契约。base 启动时优先使用本地最后一次成功供应商清单，没有缓存则使用插件内嵌供应商清单，随后尝试远端更新；因此首次离线可用内嵌供应商，已有安装断网可用缓存。内嵌清单只用于供应商身份 bootstrap，不包含全部 provider-scoped 模型数据，也不限制远端动态新增；所有来源的供应商条目仍须通过当前 base 协议与认证能力校验。没有供应商模型目录缓存时仍只适用上一条确认的 Core 元数据回退边界。
- Catalog revision 更新不会改写既有逻辑 Model 或 Provider Model；管理员现有路由与 metadata 编辑保持本地事实。
- 升级 migration 将可识别的 `ai://models.dev/*` source 一次性转换为现有 Catalog source identity；运行时不保留旧 URI alias，也不读取旧完整目录缓存。
