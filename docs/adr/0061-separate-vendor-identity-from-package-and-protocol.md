---
status: accepted
---

# Vendor 身份独立于实现包与协议

Vendor 使用独立、稳定的供应商接入身份，不再由 npm package、Provider Catalog 条目或 Protocol 决定。一个 Vendor Plugin 软件包实现一个 Vendor，可包含多个 channel；多条 Provider 连接可以引用同一 Vendor，多个 Vendor Plugin 可以复用同一个 Protocol Codec。npm package 仅作为实现来源或目录元数据，更换 SDK、共享标准 codec 或升级插件不应改变已有连接所引用的 Vendor 身份。

本决策取代 [ADR-0055](0055-key-vendors-by-npm-package.md)。保留 npm 身份可以减少同包适配实现的重复，但会将需要独立发布的供应商行为绑在一起；选择独立 Vendor 身份，将实现复用放在共享库中，而不是通过合并身份实现。

本 ADR 记录已确认的目标模型，尚不表示运行时与持久化迁移已经完成。插件标识的具体格式、来源信任、版本选择和加载生命周期另行决策。
