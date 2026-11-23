---
status: accepted
---

# Vendor Plugin 携带锁定版本的 codec

`stravia-protocol-codec` 集中维护四类标准 Protocol Codec：OpenAI-compatible、Anthropic、Gemini 与 Open Responses，以及它们共用的 canonical 转换辅助。需要这些实现的 Vendor crate 在 Rust 源码层直接依赖该库，构建时静态编入各自的自包含 Wasm Component；实现采用源码链接，不采用 Component composition。安装不另行获取 codec 包，运行时也不绑定宿主提供的可变 codec 实现，因此插件采用的协议行为只随整个插件产物更新。

私有 codec 由对应插件拥有，不由标准库或宿主反向依赖。跨插件共用的中立辅助代码可置于共享库，认证、额度和私有协议编排仍在所属 guest。普通发行版默认只内嵌 `base`；使用相同源码依赖不要求组件一起安装或更新。

不采用宿主共享 codec：它可以减少插件产物重复并集中分发修复，但会引入宿主与插件行为耦合，或要求宿主维护多版本 codec。选择自包含交付，接受代码体积重复以及 codec 修复需要重新构建、发布并更新所有受影响插件的成本；共享源码仍是标准实现的唯一维护来源。宿主可以保留协议身份并接受 guest 选择的 opaque 上游协议标识，但不得要求宿主自身拥有相应供应商 codec。

自包含不保证与任意宿主版本兼容；WIT 准入和插件数据兼容性仍独立校验，见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)。
