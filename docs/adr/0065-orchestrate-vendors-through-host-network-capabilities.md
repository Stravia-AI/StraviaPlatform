---
status: accepted
---

# 插件编排供应商流程，宿主执行受控网络操作

Vendor Plugin 主动调用宿主提供的受控 HTTP / WebSocket Interface，拥有供应商端点、编码、解析及多步辅助调用的编排。宿主拥有实际连接、代理、目标访问限制、取消、deadline 和诊断；推理、OAuth、模型发现及额度查询共用网络能力，但分别受对应操作的权限与生命周期约束，不开放绕过宿主的原生 socket。

不采用每一步返回 RequestPlan、由宿主发送后再次驱动插件的统一模式：它让宿主控制直观，却迫使 Devin AssignModel、自定义 OAuth 等真实多步流程拆成跨调用状态机。让插件编排这些流程能够集中供应商知识，代价是必须为异步宿主调用、流式背压、取消和资源回收定义明确契约。

本决定保留 [ADR-0013](0013-own-provider-transport-behind-model-turn-executor.md) 的平台职责：Model Turn Executor 继续拥有模型请求重试与 Target failover，宿主继续拥有网络执行，Inference Run 继续拥有 Hook、历史与交付。插件发起供应商调用不等于获得自行重放生成请求的权力。

该决定扩展 [ADR-0045](0045-own-request-construction-by-vendor-purpose.md) 所描述的旧 Vendor 请求构造职责：完整 Vendor Plugin 可以发起受控调用并解析模型列表，不再限于产出 URL 与 headers；请求用途明确、认证知识集中及代理策略由宿主统一执行的原则保留。模型来源优先级与失败回退不由本决策重新定义。

本 ADR 记录目标设计，尚不表示实现已完成。网络目的地授权与宿主 Interface 的具体版本另行决策。
