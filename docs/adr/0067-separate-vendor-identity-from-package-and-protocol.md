---
status: accepted
---

# Vendor 身份独立于实现包与协议

Vendor 使用独立、稳定的实现身份，不再由 npm package、Provider Catalog 条目或 Protocol 决定。最终随程序交付恰好五个 Vendor：`base` 回退 Vendor，以及 `openai-codex`、`xai-grok`、`command-code`、`devin` 四个专属 Vendor。`base` 是一个 Vendor 和一个软件包，不是把多个 Vendor 合并到同一包；它通过多个 Provider Profile 承接四个专属接入之外的全部现有供应商能力，包括既有认证、OAuth、云协议、模型发现、额度和供应商差异。普通 `openai` 与 `xai` Profile 仍归 `base`，Codex 与 Grok 则分别使用独立的 `openai-codex` 与 `xai-grok` 身份。

`VendorDescriptor` 以 `kind` 区分 `fallback` 与 `dedicated`，并在 `providers` 中声明 `ProviderDescriptor`。每个 Profile 以稳定的 `provider_id` 标识供应商接入，并可通过 `catalog_id` 关联展示目录；`ProviderSnapshot.provider_id` 是 `stravia:vendor@0.2.0` 的必填输入。供应商 Profile ID 与已保存 Provider 连接的数据库 UUID 是不同概念：迁移 Vendor 归属不能改变连接 UUID、凭据、Route 或历史。更换 SDK、共享标准 codec 或升级插件也不应改变已有连接所引用的身份。

专属 Profile 对其供应商接入进行整体接管：该身份下的全部 channel、操作和连接只能由对应专属 Vendor 执行。专属包未安装、缺少某项能力或 channel、加载失败或执行失败时，宿主都必须明确失败，不得按能力、channel 或单次操作隐式回退到 `base`。`base` 只按自身声明的 Profile 分派，不表现为多个 Vendor；专属 Vendor 只接受与自身 Vendor ID 相同的唯一 Profile。

本决策取代 [ADR-0055](0055-key-vendors-by-npm-package.md)，并明确修正本 ADR 先前“一个软件包只能表达一个供应商接入身份、复用只能进入共享库”的过窄表述。最终模型仍保持一个包一个 Vendor 实现身份，但允许 `base` 在该单一身份下声明多个供应商 Profile；四个需要独立发布和整体接管的接入则保持独立包。实现复用放在共享标准 codec 与公共辅助库中，不能通过运行时回退混用两个 Vendor 的行为。

身份、Profile 选择与 channel 准入由版本化 WIT 描述符和连接快照确定；来源信任、版本切换及不兼容数据处理继续遵循插件生命周期决策。
