---
status: accepted
---

# 用 rama 与最小 rustls 补丁承载 Vendor 出站

Vendor 出站改由 rama 的 HTTP 传输和 rustls 承载，以控制 HTTP/1 字段拼写及顺序、HTTP/2 初始帧和 TLS ClientHello。WebSocket 使用同一连接策略完成握手，再交给现有支持 deflate 的帧引擎。其他子系统继续使用 reqwest；不为迁移供应商出站同时替换整个进程的 HTTP 栈。

rustls 仅添加默认关闭的 ClientHello 配置入口。仓库保存补丁、上游归档校验和与准备脚本，不保存整份第三方源码；所有 Cargo 入口先准备独立生成目录。补丁覆盖进程中兼容的 rustls 0.23 依赖，因此必须验证关闭入口时行为不变，并在安全更新时重放补丁及握手验证。

允许经白名单审查的仅声明算法，但服务端选择无实现的算法时必须中止握手；不得伪造密钥交换、验签成功或关闭证书验证。信任来源沿用平台验证器，shape 不授予额外信任。HTTP、HTTPS、SOCKS5 与远端 DNS 代理的既有行为保持一致。

未选择 hyper 的私有字段旁路、完整 wreq 客户端、libcurl 或 Go/uTLS：前者不能提供所需公共控制面，其他选项引入不同的连接或运行时体系。rama 的 0.x API 变化和并存两套 HTTP 栈是已接受的代价。本决定局部替代 [ADR-0013](0013-own-provider-transport-behind-model-turn-executor.md) 和 [ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md) 对 reqwest 实现位置的描述，不改变重试及观察的权限与生命周期。
