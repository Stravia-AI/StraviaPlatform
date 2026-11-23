---
status: accepted
---

# 用 Egress Shape 描述客户端出站形态

## 背景与决策

Egress Shape 是可按名称选择的连接层形态，包括 TLS ClientHello、ALPN、HTTP/1 字段位置、HTTP/2 参数与 WebSocket 握手。它不代替 Provider Profile、业务 headers、身份凭据或重试。内置形态使用裸名，插件自有形态使用 `plugin:<local-name>`；未知名称失败。插件构造业务字段，宿主验证协议必需的 Slot Header 并在声明位置填值。宿主不为贴合客户端自行添加业务字段。

连接和 TLS 会话复用按形态及修订隔离，同时考虑插件组件身份、代理与目标。插件形态和组件一起准入、编译与固定；在途请求继续使用原快照。形态不能覆盖内置名称、修改信任根或扩大网络授权。采样只说明记录的平台、版本与字段，不承诺 TCP/IP、时序或端到端相同。

## 权衡

将所有形态编入宿主会让独立 Vendor 更新依赖宿主发版；让插件带独立网络栈又失去宿主对授权、证书和资源的控制。插件形态可以随包发布，但只能声明宿主已有的传输原语，由宿主验证并执行。此决策细化 [ADR-0071](0071-orchestrate-vendors-through-host-network-capabilities.md)；完整配置见 [WIT 契约与受控出站网络](../design/vendor-plugins.md#wit-契约与受控出站网络)。
