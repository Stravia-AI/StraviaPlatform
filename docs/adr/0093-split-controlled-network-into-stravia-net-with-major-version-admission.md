---
status: accepted
---

# 分离受控网络契约并按主版本准入插件

## 背景与决策

受控 HTTP / WebSocket imports 属于独立的 `stravia:net`，Vendor 操作与私有存储不借网络 options 扩展；原子状态有单独的 `stravia:state`。网络选项通过开放键值表达，不支持的键或值显式拒绝。存储 scope、租约、代际和历史来源由宿主控制，不允许 guest 任意寻址 Provider 或 Principal。

对已发布 WIT 保持同主版本下的函数、resource 与类型布局。宿主按接口和稳定版本准入，不接受高于支持 minor 的 imports；Wasmtime 继续检查实际链接与必需导出。同主版本不是任意接口的授权，新增必需导出也不是兼容升级。发布快照需保持不变，兼容性不能只靠比较版本字符串。

## 权衡

把网络字段继续写进每一版 Vendor WIT 会让连接选项变更迫使插件重建；开放选项配合明确拒绝可演进配置而不模糊 ABI 或权限。该决策细化 [ADR-0072](0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)；支持版本、状态和限额见 [WIT 契约与受控出站网络](../design/vendor-plugins.md#wit-契约与受控出站网络)。
