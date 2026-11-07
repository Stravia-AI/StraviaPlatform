---
status: accepted
---

# 用 rama 与最小 rustls 补丁承载 Vendor 出站

Vendor 出站改由 rama 的 HTTP 传输和 rustls 承载，以控制 HTTP/1 字段拼写及顺序、HTTP/2 初始帧和 TLS ClientHello。WebSocket 使用同一连接策略完成握手，再交给现有支持 deflate 的帧引擎。其他子系统继续使用 reqwest；不为迁移供应商出站同时替换整个进程的 HTTP 栈。

rustls 仅添加默认关闭的 ClientHello 配置入口。仓库保存补丁与固定依赖声明，不保存整份第三方源码；所有 Cargo 入口先准备独立生成目录。补丁覆盖进程中兼容的 rustls 0.23 依赖，因此必须验证关闭入口时行为不变，并在安全更新时重放补丁及握手验证。

实际交付还补丁化 rama-http-types 和 rama-http-core，以支持有序字段及传输编码。使用固定版本的 [patch-crate](https://crates.io/crates/patch-crate) 管理补丁，删除自维护的下载、解包、应用和修订目录管理器：根 `Cargo.toml` 的 `workspace.metadata.patch` 声明目标，`patches/<crate>+<version>.patch` 保存改动，`patch.crates-io` 指向 `target/patch/<crate>-<version>`。原始 registry 源校验沿用 Cargo，Nix 离线输入另由固定哈希约束。这些补丁不只是 rustls 补丁的附属配置，升级 rama 时也需维护和验证。

`task prepare:crates` 在输入变化或产物缺失时调用 `cargo patch-crate --force`，避免上游默认跳过已有目录而留下旧补丁；未变化时由 Task 复用产物。这不再承诺旧准备器的不可变修订目录与原子发布语义：刷新会重建生成目录，必须在构建前执行，不能与构建并发。准备失败必须停止构建；再次准备从干净生成目录开始。接受这一准备阶段约束，换取不再维护第二套 Cargo 源码获取与补丁引擎。

允许经白名单审查的仅声明算法，但服务端选择无实现的算法时必须中止握手；不得伪造密钥交换、验签成功或关闭证书验证。信任来源沿用平台验证器，shape 不授予额外信任。HTTP、HTTPS、SOCKS5 与远端 DNS 代理的既有行为保持一致。

Wire Debug 的请求头是按 rama 序列化规则构造的应用层快照，不是对套接字字节或 HTTP/2 帧的逐字节抓包；协议归一化与必要的 framing 会改变插件提交的字段。实际网络行为及指纹保真须由独立 wire capture 验证，不能由此 ADR 推断测试结果。

未选择 hyper 的私有字段旁路、完整 wreq 客户端、libcurl 或 Go/uTLS：前者不能提供所需公共控制面，其他选项引入不同的连接或运行时体系。rama 的 0.x API 变化和并存两套 HTTP 栈是已接受的代价。本决定局部替代 [ADR-0013](0013-own-provider-transport-behind-model-turn-executor.md) 和 [ADR-0063](0063-record-four-direction-wire-debug-at-transport-boundaries.md) 对 reqwest 实现位置的描述，不改变重试及观察的权限与生命周期。
