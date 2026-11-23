---
status: accepted
---

# 可逆脱敏只在本地检测凭据

Stravia 在本地识别凭据，不向外部服务发送候选秘密验证。内置 Betterleaks 规则随版本交付，不在运行时下载；本地命中无需证明凭据真实有效。联网验证虽能减少部分误报，但会扩大秘密接收方，也会引入外部审计、限流和网络故障，因此不采用。自定义规则同样不得绕过本地检测边界。

规则依赖上下文，仅重新扫描会漏掉已识别秘密在其他位置的裸值，因此也使用当前 API Key 下仍可用于出站保护的已知映射；现代规则门控、强弱匹配与到期条件见 [ADR-0090](0090-gate-credential-protection-by-rule-and-strength-and-expire-mappings.md)。凭据占位符本身是协议原子，不参与新秘密检测。

扫描、替换或必要映射存储失败时，终止受保护请求，不降级发送原文；扫描完成且无命中不算失败。该保护不是诊断存储的保密保证。完整契约分别由[凭据保护](../design/credential-protection.md)与[请求记录](../design/interaction-observation.md)拥有。
