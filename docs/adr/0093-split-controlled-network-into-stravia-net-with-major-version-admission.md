---
status: accepted
---

# 分离受控网络契约并按主版本准入插件

Vendor 1.0 将受控 HTTP / WebSocket imports 移入独立的 `stravia:net@1.0.0`；`stravia:vendor@1.0.0` 只保留供应商操作、事件和私有状态。网络参数通过 options resource 的开放键值传递，未知键或不支持的值显式失败，不能悄悄忽略。

同一主版本内保持已发布的函数、resource 和类型结构不变，新增网络参数不改变 WIT。宿主仅允许列入清单的接口和稳定版本，并拒绝高于自身支持 minor 的 imports；Wasmtime 继续校验实际类型和函数。新增必需导出不是兼容升级。发布快照与真实旧 Component 的加载检查共同保护兼容性，不能只比较版本字符串。

准入清单按接口维护：`vendor/host` 与 `vendor/types` 支持稳定 1.0.x；`net/types`、`net/http`、`net/websocket` 与 `state/store` 支持已发布的 1.0.x 和 1.1.x；1.1 新增的 `state/journal`、`state/volatile` 不能声明为 1.0。WASI 0.2.x interfaces 仍另行列出，build metadata 不影响版本比较。准入不是对任意同主版本接口的授权，链接和必需导出仍须匹配。快照保存在 SDK 的 `wit/released/`；兼容检查与真实旧 Component 加载共同验证，不覆盖或重写已发布快照来通过检查。

原子状态独立为 `stravia:state`，避免扩充网络 options 承担存储职责。1.0 固定 provider／conversation cell 与 revision CAS；1.1 新增 ContentStore-backed journal 和进程易失状态，不添加旧 scope 枚举成员。Net 1.1 只新增真实 `is-fresh-handshake` 事实；Cookie 与本地 reuse-only checkout 仍是开放 options 键。存储 scope、租约、代际与历史来源由宿主选择，guest 不获得任意 Provider／Principal 标识寻址能力。具体生命周期和限额见 [Vendor 插件设计](../design/vendor-plugins.md#state-1x-与原生请求状态)。

1.0 是干净切换：0.6 独立插件必须重建并重新导入，不保留旧 ABI adapter。WASI 的既有准入、网络权限、取消、deadline、资源限额和宿主重试职责不变。本决定细化 [ADR-0072](0072-use-wasmtime-components-and-wit-for-vendor-plugins.md)，不扩大插件获得的权限。
