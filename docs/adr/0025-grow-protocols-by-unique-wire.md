---
status: accepted
---

# Grow Protocols by unique wire contract

Protocol 按 HTTP path、request/response body 与 stream wire contract 去重，而不是按 Vendor 或 npm package 命名。已有 wire 必须复用现有 Protocol codec；只有当前 npm source 证明 wire 不同，才新增 egress Protocol，并让它完整通过 canonical ProtocolTransform 与 Representability gate。这个规则取代 ADR-0006 中协议集合固定为四条的约束。
