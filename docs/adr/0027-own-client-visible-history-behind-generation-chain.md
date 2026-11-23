---
status: accepted
---

# Own client-visible history projection and node legality behind Generation Chain

## 背景与决策

Generation Chain 保存 ingress 的客户端可见投影，而不是 Effective Model Request。链自身负责 item reference、tool ID 与历史结构投影，并判断输出是否有资格形成本地节点；Inference Run 在完整交付后提交。Protocol Conversion 仍只承担绑定协议对的 decode、encode 与 stream，不暴露链历史操作。

## 权衡

把投影留在 codec 看似更接近 wire，却会把链引用与持久合法性暴露给无关调用方；只搬动投影、不收拢写入判定，又会让各交付路径重复判断终态。把两者放在 Generation Chain 内，保证写入与自动发现使用同一个客户端视图。当前协议投影见 [Open Responses](../design/open-responses.md)，物化边界见 [数据布局与性能](../design/data-layout-performance.md)。
