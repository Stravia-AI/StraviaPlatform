---
status: accepted
---

# Vendor Plugin 携带锁定版本的 codec

`stravia-protocol-codec` 集中维护四类标准 Protocol Codec：OpenAI-compatible（包括 embeddings）、Anthropic、Gemini 与 Open Responses，以及它们共用的 canonical 转换辅助。需要这些实现的 Vendor crate 在 Rust 源码层直接依赖该库，构建时静态编入各自的自包含 Wasm Component；实现采用源码链接，不采用 Component composition。安装不另行获取 codec 包，运行时也不绑定宿主提供的可变 codec 实现，因此插件采用的协议行为只随整个插件产物更新。

随程序交付的最终实现是 `stravia-vendor-base`、`stravia-vendor-codex`、`stravia-vendor-grok`、`stravia-vendor-command-code`、`stravia-vendor-devin` 五个 crate。Codex 与 Grok 可复用标准 codec；Command Code 与 Devin 的私有 codec 分别归各自 guest crate；Bedrock、Cohere、Gateway、WatsonX 等非标准实现归 `stravia-vendor-base`。标准库和宿主构建不得反向引用这些供应商私有 codec。跨插件共用且不包含供应商流程的辅助代码由 `stravia-vendor-common` 提供，认证、额度和私有协议编排仍留在所属 guest。

不采用宿主共享 codec：它可以减少插件产物重复并集中分发修复，但会引入宿主与插件行为耦合，或要求宿主维护多版本 codec。选择自包含交付，接受代码体积重复以及 codec 修复需要重新构建、发布并更新所有受影响插件的成本；共享源码仍是标准实现的唯一维护来源。宿主可以保留协议身份并接受 guest 选择的 opaque 上游协议标识，但不得要求宿主自身拥有相应供应商 codec。

该决定不保证插件能在任意宿主版本上运行；`stravia:vendor@0.2.0` Interface 兼容性与插件数据兼容性仍须独立校验。
