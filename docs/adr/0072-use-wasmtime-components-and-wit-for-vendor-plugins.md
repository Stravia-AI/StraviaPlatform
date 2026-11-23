---
status: accepted
---

# 使用 Wasmtime Component Model 与 WIT 统一 Vendor 插件契约

Vendor Plugin 使用 Wasmtime 执行 WebAssembly Component，以版本化 WIT 定义供应商导出能力和受控宿主 imports。发布单个自包含 Component，携带锁定的 codec 依赖；首先提供 Rust SDK 迁移既有实现，但不将契约限定为 Rust。

不同时支持 Extism 或自定义 Core Wasm ABI，避免维护两套供应商契约；不直接暴露内部 Rust trait、Gateway 或数据库对象。相较自行管理指针、内存释放和跨语言类型，WIT 提供显式类型与资源契约，代价是需要维护接口版本并验证 guest 工具链、异步流式、取消与资源回收。

本决策选择执行与接口模型，不作运行性能承诺。网络接口拆包、主版本准入和发布快照以后续 [ADR-0093](0093-split-controlled-network-into-stravia-net-with-major-version-admission.md) 为准，现行技术契约见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)。
