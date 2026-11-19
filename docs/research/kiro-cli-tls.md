# Kiro CLI 3.4.0 TLS 出站形态

调研日期：2026-11-18。此记录仅用于兼容性研究，不用于绕过访问控制或仿冒设备、安装标识。

## 测量方法与范围

使用官方下载的 Linux x86_64 3.4.0 包内 KAS ACP server 和其 Node.js 22.23.4，KAS 报告版本 0.66.111。ACP 驱动以合成 `kiro-cli` clientInfo 初始化，创建会话，选用本机 stub 提供的合成模型并发送合成提示。临时 HOME、XDG 和工作目录，`KIRO_API_KEY` 仅为合成占位值；明确指定区域，并把 runtime 和 control-plane endpoint 均覆盖到 `https://localhost`。客户端以 `NODE_EXTRA_CA_CERTS` 信任本机临时证书，并非禁用证书验证。进程在 Linux 用户及网络命名空间内运行，仅开启 loopback，未连接公网模型或真实账号。

本机 TCP 接收器记录客户端发出的原始 TLS 首次数据，并将字节原样转发给同一连接上的本机 HTTPS stub；stub 识别目标操作，返回合成模型目录或空事件流。接收器观察到 `KiroControlPlaneBearerService.ListAvailableModels` 和 `KiroRuntimeService.GenerateAssistantResponse`，以及启动期间其他 management/runtime 操作；空事件流导致生成操作重试。十条 TLS 连接的脱敏 ClientHello 字段一致，因此此样本覆盖上述两个操作，但不是跨运行、平台或版本的稳定性证明。ALPN 扩展未出现，服务端也没有协商到 ALPN；在该本机实验中请求使用 HTTP/1.1。没有记录请求体、授权值、TLS 随机数、会话 ID、密钥交换字节或证书私钥。

脱敏 fixture [`kiro-cli-3.4.0-tls.json`](../../backend/crates/stravia-vendor-kiro/kiro-cli-3.4.0-tls.json) 使用 `capture-client` 的 `clienthello` 字段与 `verify-egress-shape` 比较约定，但此次使用专门的本地 TCP 记录器和独立解码脚本，而不是声称由 `capture-client` 生成。仅保留协议字段、允许的扩展内容、SNI 存在性与长度；原始 ClientHello 不入库。`h1`、`ws` 和 `body_pattern` 为 null，不声称完整 HTTP 字段已测量。结果为：

- ClientHello legacy version 771；supported versions TLS 1.3、1.2；SNI 存在。
- 扩展顺序 `65281,0,11,10,35,22,23,13,43,45,51`；无 ALPN（不是显式声明 `http/1.1`）。
- groups 为 `X25519MLKEM768, X25519, secp256r1, X448, secp384r1, secp521r1, ffdhe2048, ffdhe3072`；key shares 为前两项，长度分别为 1216、32。
- JA3 MD5 `944d1e1858cd278718f8a46b65d3212f`；JA4 `t13d521100_b262b3658495_8e6e362c5eac`。完整密码套件、签名算法和安全扩展事实见 fixture。

## 已授权的公网 TLS 验证

使用已登录的官方 3.4.0 CLI 发起 ACP 无工具短文本调用，结果为 `end_turn`。首个透明探针未观测到握手。随后在 CLI 使用的 HTTPS 代理路径放置只允许两个美东 Kiro 域名的本地 CONNECT 转发器，接入原有出站代理；转发器不终止 TLS、不新增 CA，也不改变证书验证。只在内存解析客户端首个明文握手 record，输出匹配结果，不写出随机数、session、key share 原始值、认证头或 TLS 密钥。

`management.us-east-1.kiro.dev` 的 3 次与 `runtime.us-east-1.kiro.dev` 的 4 次 ClientHello，逐一与上述 fixture 比较。七次的版本、完整密码套件及扩展顺序、支持组、签名算法、版本列表、点格式、PSK 模式、安全扩展字段、空 ticket、key-share 组及长度均相同，无 ALPN 扩展；SNI 各自等于 CONNECT 目标（与 localhost 名称长度不同）。这确认了该平台、版本、账号和代理路径的七次新握手，不覆盖 TLS 恢复、所有区域或网络，也不意味着不同软件栈的握手字节完全相同。

[交互式 TUI 直连实测](kiro-cli-request-alignment.md#交互式-tui-实测)另确认目录、额度与三次主推理均协商 TLS 1.3，未协商 ALPN。TUI 的 HTTP 记录是主协议基准；上面的完整 ClientHello 参考样本来自 KAS/ACP，不能说成重新捕获了 TUI ClientHello。

## 声明与离线验证

插件独立声明 [`egress-shape.json`](../../backend/crates/stravia-vendor-kiro/egress-shape.json)，使用受支持的 WIT shape 字段；按捕获值配置空 HTTP ALPN、TLS 顺序、组和签名算法。不能实际协商的密码套件按宿主现有约定只作 advertise-only，部分组也只被声明而没有安装为可执行 key exchange；这不等于完整复刻官方客户端所有服务端选择分支。shape 不提供认证、域名权限、证书信任、安装 ID 或请求头策略。声明 HTTP/2 和 WebSocket 的默认字段不代表观察到相应 Kiro 流量。

将该 JSON 解码为插件 `egress_shapes` 的 `EgressShape`，HTTP transport 选择 `plugin:kiro-cli-3-4-0`。打包 component 后可使用仓库工具离线验证：

```sh
stravia-tools verify-egress-shape \
  --component PATH_TO_KIRO_COMPONENT.wasm \
  --shape plugin:kiro-cli-3-4-0 \
  --fixture backend/crates/stravia-vendor-kiro/kiro-cli-3.4.0-tls.json \
  --server-name localhost
```

必须使用 `localhost`：本 fixture 捕获的 SNI 扩展长度依赖该名称，验证器比较扩展长度。这个离线命令比较合成 ClientHello 与所记录字段，不访问上游，不证明完整 HTTP 请求、证书校验、服务端握手选择或真实账户推理成功。该命令使用当前构建的 Kiro Wasm 返回 `{"status":"ok"}`。endpoint override、本地 CA、合成 stub 和 ACP 驱动都会影响实验环境，不能推断官方线上连接、其他区域、系统或客户端版本完全一致。上游请求内容的独立证据和限制见 [请求对齐调研](kiro-cli-request-alignment.md)。
