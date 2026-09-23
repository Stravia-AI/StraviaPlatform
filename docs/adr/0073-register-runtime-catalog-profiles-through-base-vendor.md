---
status: accepted
---

# 由 base Vendor 在运行时注册远端目录中的兼容 Provider Profile

## 迁移基线

本文所称“插件化迁移前原生基线”固定指 commit `4b7cabcbb3894d7f9e5226849e17c5b663dbf8cb`，即全量插件切换 commit `38586e756a40c98503cf43761c0e01354d31dfbe` 的直接 parent。它代表 Vendor 插件化之前的原生供应商实现；文中任何“沿用”都只继承该固定点的原生行为，不以插件化后的 `ProviderDescriptor` 或当前插件实现替代历史基线。

## 决定

`stravia-vendor-base` 在运行时消费 `https://models.stravia.cn/providers.json`，仅将能够映射到 base 已支持协议与认证实现的目录条目通过 Vendor Plugin 契约注册为 `ProviderDescriptor`。因此远端新增的兼容供应商无需更新 Stravia 或重新构建 base 即可添加；目录条目本身不代表可执行支持，未受支持的协议或认证方式不得冒充可用 Profile。本决定以运行时目录注册取代构建期静态清单作为 base 完整可用身份边界，但不新增协议或认证实现，也不改变 dedicated Profile 的整体接管及禁止合并规则。

base 离线启动供应商清单时，优先使用本地最后一次成功清单；没有缓存时使用插件内嵌清单，然后再尝试远端更新。因此首次离线仍可选择内嵌供应商，已有安装断网时仍可使用缓存中的供应商。内嵌清单只用于 bootstrap，不限制远端动态新增；无论条目来自缓存、内嵌还是远端，都必须按当前 base 已实现的协议与认证能力校验后才能注册。

供应商模型目录是对应 Provider Profile 的一部分，由 base 在运行时获取 `https://models.stravia.cn/providers/{provider_id}/models.json`；这里的 Provider 指供应商接入身份，不是已保存连接的 UUID。内嵌 bootstrap 不包含全部供应商模型数据。Core 继续持有 `https://models.stravia.cn/models.json` 的 Canonical Model 数据，并只在供应商模型目录没有数据时作为回退来源。模型集合仅由供应商发现或管理员明确添加确定；Core 不通过回退增加成员或声明模型可用，只为其中缺失元数据的模型补充 Canonical Model 数据。

模型集合已经由上游发现或管理员添加，但 base 获取供应商模型目录元数据发生超时、HTTP 500 等失败时，允许使用 Core Canonical Model 补充元数据。该回退必须明确报告供应商模型目录获取失败，不得把本次目录刷新标为成功，不得覆盖已有有效数据，也不得扩大模型集合；真正的上游账号级模型发现失败不在此授权范围内，不能用全局目录冒充成功。

base 的供应商目录与 Core 的全局 Canonical Model 目录独立刷新，不再要求跨模块等待并同时切换到同一 revision。base 新增兼容供应商不受 Core 下载失败阻塞，Core 刷新也不等待 base；接受两边短时处于不同 revision。两边分别校验各自下载的一致性，失败不得标记成功，并保留已有有效数据。

本决定部分修订 [ADR-0018](0018-use-revisioned-stravia-model-catalog.md) 中跨模块原子 revision 与“scoped 下载失败导致整个同步失败”的约束；ADR-0018 的其他缓存和模型快照契约继续有效。除本文明确变更外，目录迁移遵循本文定义的插件化迁移前原生基线；不得从插件化后的当前实现反推继承行为。该基线在 `provider_catalog/mod.rs`、`runtime.rs`、`admin/provider_connection.rs`、`admin/routes/provider_model_records.rs` 与 `provider/registry.rs` 中给出以下边界：

- 启动后立即后台刷新，之后每小时刷新；失败明确记录并保留当前有效快照。
- 成功刷新会整体替换供应商目录。远端删除供应商后，当前目录中的 channel 解析返回 `ProviderNotFound`，因此不能再从被删除的 Catalog 条目新建 Provider；已经保存的 Provider 记录不会因刷新而删除，普通推理也不会仅因该目录删项自动停用。
- 已有 Provider 使用 Catalog 同步模型时，供应商缺项原生会报告 `ProviderNotFound`，而不是当作空 scope 成功。上文确认的 Core 元数据回退是明确例外：仅当模型集合已由上游发现或管理员添加确定时补充缺失元数据，仍须报告目录失败，也不恢复已删除条目的新建资格。
- 原生元数据富化先查找 provider scope 中 upstream model ID 的精确项；未命中时，Canonical Model 先按完整 ID 精确匹配，再按最右段的 ASCII 小写键做唯一匹配，多个候选则不匹配，最后才使用 bare metadata。该匹配顺序不改变上文确认的模型集合边界。

动态新增 Profile 的持续注册方式、持久化 schema 和缓存目录属于实现设计，本轮不另设产品策略。
