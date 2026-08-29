---
status: accepted
---

# Key Vendors by npm package

Vendor 身份由 Provider Catalog 的 npm package 决定，而不是 catalog id 或 Protocol。Catalog 负责发现和展示；Vendor 负责同包 Provider 共享的鉴权、URL、headers 与 SDK-derived Adapter Credentials。这样 Azure 等多 catalog id 的同包服务共享运行时契约，同时避免让协议族或品牌名承担错误的身份职责。
