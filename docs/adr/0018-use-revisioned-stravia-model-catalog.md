---
status: accepted
---

# Use the revisioned Stravia Model Catalog as a template source

## 背景与决策

`models.stravia.cn` 是版本化 Stravia Model Catalog 的事实源。Core 消费 `/models.json` 的 Canonical Model 数据；base Vendor 运行时消费供应商清单与按需模型目录。两侧独立校验、刷新和保留最后有效数据，不要求跨模块原子切换。供应商 Profile 的具体职责见 [ADR-0073：base Vendor 运行时目录](0073-register-runtime-catalog-profiles-through-base-vendor.md)。

Canonical Model 以 `{lab_id}/{model_id}` 标识，仅为逻辑 Model 和手动 Provider Model 提供一次性模板。已导入的快照保持本地所有权；目录变更不覆盖管理员编辑。不从近似名称猜测模型，不用 Core 元数据回退扩大管理员或上游已确定的模型集合。

## 权衡

下载整库和强制两个消费者共用 revision 会让独立刷新、按需获取和离线启动相互阻塞。独立校验接受短时 revision 差异；目录不可用时可保留已有有效数据，但不能把失败当作新目录成功。快照更新和字段钉住见 [ADR-0084](0084-pin-provider-model-fields-individually.md)；当前目录与快照契约见 [系统架构](../design/architecture.md)。
