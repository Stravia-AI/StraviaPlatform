---
status: accepted
---

# Grow Protocols by unique wire contract

Protocol 按 HTTP path、request/response body 与 stream wire contract 去重，而不是按 Vendor 或 npm package 命名。已有 wire 必须复用现有 Protocol codec；只有 HTTP、消息或流的实际 wire 不同时才新增 egress Protocol；包来源不是准入条件。新增协议仍须通过 canonical ProtocolTransform 与 Representability 边界。这个规则取代 ADR-0006 的固定协议集合；当前转换契约见[整体架构](../design/architecture.md)。
