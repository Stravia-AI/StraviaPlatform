---
status: accepted
---

# 使用 Wasmtime Component Model 与 WIT 统一 Vendor 插件契约

Vendor Plugin 使用 Wasmtime 执行 WebAssembly Component，以版本化 WIT 定义供应商导出能力和受控宿主 imports。发布单个自包含 Component，携带锁定的 codec 依赖；首先提供 Rust SDK 迁移既有实现，但不将契约限定为 Rust。

不同时支持 Extism 或自定义 Core Wasm ABI，避免维护两套供应商契约；不直接暴露内部 Rust trait、Gateway 或数据库对象。相较自行管理指针、内存释放和跨语言类型，WIT 提供显式类型与资源契约，代价是需要维护接口版本并验证 guest 工具链、异步流式、取消与资源回收。

这是技术栈决策，不是运行或性能验证结果。具体依赖版本与 WIT 异步特性选择尚未确定，宿主和 guest 的可用性需通过真实插件调用验证。
