---
status: accepted
---

# 所有模型 Vendor 统一使用 Wasm 插件

Stravia 通过同一 Wasm Component 通道加载和执行所有模型 Vendor，不维持“内置原生、外部 Wasm”的两套执行语义。双通道会使复杂认证、私有协议或管理能力只在内置实现可用。供应商插件拥有自身流程；宿主仍拥有通用网络、存储、授权、调度、重试、Target failover 与客户端协议处理。

预装与外部安装是分发差异，不是能力契约差异。普通发行版默认只内嵌 `base`；专属包可经本地导入或插件市场安装，不因此随宿主默认更新。构建时内嵌、覆盖与恢复规则见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)，安装来源与网络分发见[插件市场设计](../design/plugin-marketplace.md)。

插件字节是按内容寻址的受管文件，关系库只保存元数据和状态。不把数据库备份当成完整插件备份：恢复还需要与之匹配的插件产物。具体原子安装、存储与备份边界见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)与[存储设计](../design/data-layout-performance.md)。
