---
status: accepted
---

# 分离受控网络契约并按主版本准入插件

Vendor 1.0 将受控 HTTP / WebSocket imports 移入独立的 `stravia:net@1.0.0`；`stravia:vendor@1.0.0` 只保留供应商操作、事件和私有状态。网络参数通过 options resource 的开放键值传递，未知键或不支持的值显式失败，不能悄悄忽略。

同一主版本内保持已发布的函数、resource 和类型结构不变，新增网络参数不改变 WIT。宿主仅允许列入清单的接口和稳定版本，并拒绝高于自身支持 minor 的 imports；Wasmtime 继续校验实际类型和函数。新增必需导出不是兼容升级。发布快照与真实旧 Component 的加载检查共同保护兼容性，不能只比较版本字符串。

1.0 是干净切换：0.6 独立插件必须重建并重新导入，不保留旧 ABI adapter。WASI 的既有准入、网络权限、取消、deadline、资源限额和宿主重试职责不变。本决定细化 [ADR-0072](0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)，不扩大插件获得的权限。
