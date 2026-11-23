# 管理认证与首次设置

## 管理身份与会话

Admin User 管理实例；推理与 MCP 使用 API Key 及其 Principal。两套身份不互换，API Key 不归属管理用户。
Core 通过 `AdminAuthProvider` 验证管理会话，以 `AdminAuthorizer` 判定管理权限；传输适配器负责提取凭据，不替代授权。见 [ADR-0040](../adr/0040-separate-admin-identity-and-revoke-sessions.md)。
每实例只有一个具备全部管理权限的 `admin`。Server 支持用户名和密码登录、退出和修改凭据，不支持公开注册或邮件找回。

- Access JWT 有效期为 15 分钟，轮换 refresh token 可续期，登录总期限最多 7 天。验签后还须检查服务端会话。
- 退出撤销当前会话；改密或本地恢复撤销全部旧会话，旧 access 和 refresh 均失效。会话和签名密钥存于所选数据库，普通重启不撤销有效会话。
- 认证存储故障返回 `503`；撤销未提交不清 Cookie、不报告退出成功。WebUI 只有收到 refresh 的明确 `401` 才回到登录；网络错误、`403` 和服务端错误保留地址及查询参数，提供重试。
- Server 使用 `HttpOnly`、`SameSite=Strict` 的 `stravia_access`（`Path=/`）和 `stravia_refresh`（`Path=/api/v1/auth`），HTTPS 再加 `Secure`。不在响应正文或 localStorage 交付令牌。
- `GET /api/v1/auth/state` 只报告 `setup`、`server`、`desktop` 或 `unavailable` 与请求资格，不刷新会话。Web 写请求要求精确同源的 `Origin`、`X-Stravia-CSRF: 1`，有请求正文时要求 JSON。

Server 的日常写接口为 `/api/v1/auth/login`、`/api/v1/auth/refresh`、`/api/v1/auth/logout` 与 `/api/v1/auth/credentials`。
入口／代理拒绝时，页面提示检查入口配置，并重试原地址；不通过清凭据或改地址掩盖部署问题。

### Desktop

首次启动使用本地 SQLite 并自动创建唯一管理员，后续复用身份。原生通道在内存中交付 access JWT，管理 HTTP 使用 Bearer；refresh token 留在原生进程内存。
会话到达总期限后可由原生通道重新建立。普通浏览器或本机进程不因回环地址获得管理权限。
Desktop 不设默认密码，也不开放密码登录。将数据库交给 Server 时，使用本地恢复命令设置凭据。
实际退出应用结束当前原生会话；关闭窗口到托盘不算退出，下次启动自动登录。

## 启动与迁移进度

Server 完成参数、入口策略和数据目录检查后先监听，再准备数据库与 Gateway。准备期间提供内嵌 WebUI、`GET /api/v1/startup` 快照和 `GET /api/v1/startup/events`；业务 API、`/healthz` 和 `/readyz` 返回 `503`。
已配置实例用同一次存储准备检查管理员并启动 Gateway，首次设置同样复用准备结果；普通启动不做全库审计。迁移约束见[数据布局](data-layout-performance.md#升级迁移原则与约束)。

- Core 报告 `starting`、`ready`、`failed`。`startup` SSE 先发当前快照，只保留最新状态并在终态后结束；接口禁止缓存，不返回 SQL、路径、凭据或原始异常，也不授予管理权限。
- 中英文界面显示真实阶段。总量可知时显示该阶段的完成数、总数和百分比；未知总量显示不定进度，不估算整体百分比。
- 数据批次提交、诊断文件可靠落盘或现有文件验证通过后才推进计数。重启续跑按本次待处理工作计数。
- `ready` 仅表示启动准备完成；浏览器随后检查认证与设置状态，不把它当作首次设置完成或模型入口可用的证明。
- 连接失败提供刷新；启动失败提示检查日志后重启，不跳转登录掩盖故障。Server 初始化失败仍以错误退出。
- Desktop 通过原生快照和事件桥展示进度及恢复入口，不依赖尚未启动的 HTTP。

## Server 首次设置

### 准入与 Setup Token

数据库中是否已有管理员是唯一完成依据。没有数据库配置时允许选择数据库；已配置且可读但无管理员时允许设置；已有管理员时仅正常登录。
配置损坏、连接失败或 schema 不兼容明确报错，不当作全新安装，也不创建替代库。设置完成前不提供推理或 MCP。

- 启动时向控制台输出一次性 Setup Token。`POST /api/v1/setup/claim` 首次成功即消费，最多一个请求领取。
- 领取获得 `stravia_setup`（`HttpOnly`、`SameSite=Strict`、`Path=/api/v1`；HTTPS 加 `Secure`）。
  设置会话只授权 `POST /api/v1/setup/test` 和 `POST /api/v1/setup/complete`，不授权管理 API。
- 令牌与设置会话无进程内时限；连接测试失败可修正。未完成时重启发新令牌，旧令牌及会话失效；完成后关闭设置权限。
- 控制台日志若持久化了未消费令牌，持有者在该进程完成设置前仍可能领取，须保护日志访问。

### 配置、提交与恢复

数据库选项只取 `server.toml`，默认位于 `<data-dir>/server.toml`，可用 `--config` 指定。`--data-dir` 独立确定托管路径，不选择后端。
`[database]` 的 `backend = "sqlite"` 使用固定 `<data-dir>/db/gateway.db`；`backend = "postgres"` 连接操作者已创建的库，运行 Stravia schema 迁移，不需要 `CREATEDB`。PostgreSQL `url` 为秘密，须保护文件权限。见 [ADR-0041](../adr/0041-own-database-connection-in-config-file.md)。

1. 验证连接和配置目录可写，再原子写入连接配置；保存失败不创建管理员。
2. 完成迁移并在数据库事务中创建唯一管理员。接入已有管理员的库时不覆盖账户，直接关闭设置入口并进入正常登录。
3. 同一进程开放 Gateway，浏览器使用管理员凭据登录；设置会话不会升级为管理会话。

配置与数据库不是跨系统原子事务：账户创建失败保留配置，重启后按同一目标库恢复；账户已提交但客户端没收到响应时，继续正常登录，不删库或重复创建账户。

本地恢复：先停止占用同一数据根的实例，再执行 `stravia-server --data-dir <root> --config <path> recover-admin`。
命令要求配置、数据库及既有管理员可读，在终端无回显读取新密码，原地更新账户并撤销旧会话；不开放设置向导，不从命令行或日志接收密码。

恢复操作只改管理凭据和会话，不影响业务数据。认证扩展仍通过 [Core 的管理身份接口](../../backend/crates/stravia-core/src/admin/identity.rs)。

## 管理入口与代理信任

Server 传输层恢复外部源并执行入口策略。HTTP、HTTPS 都允许，默认监听 `127.0.0.1:23471`；模型 API、MCP 与健康探针不使用管理入口列表。见 [ADR-0052](../adr/0052-allow-explicit-http-admin-origin.md)。

- 重复的 `--admin-origin` 或逗号分隔的 `STRAVIA_ADMIN_ORIGINS` 定义整个管理面允许的协议、主机和有效端口；未配置则不限。拒绝通配符、凭据、路径、query、fragment 和非法或显式空项；不按未受信 Host 重定向。
- `--trusted-proxy` / `STRAVIA_TRUSTED_PROXIES` 独立配置 IP/CIDR，默认不信任代理，仅匹配直接 TCP 对端。非法配置使启动失败，修改部署配置后重启恢复。
- 非受信对端的转发声明不参与恢复，按直连 Host 和 HTTP 处理。受信代理必须清理客户端声明，并可提供唯一配对的 `X-Forwarded-Proto` 和 `X-Forwarded-Host`；两者都无则按直连，缺一、重复、逗号链、冲突或 RFC `Forwarded` 均拒绝。
- `Origin` 是被校验对象，不是外部源的依据；两个允许入口也不能互相跨源写入，不开放管理跨源 CORS。
- Cookie 的设置和清除按恢复出的外部协议决定 `Secure`，HTTPS 代理的 HTTP 回源不降低保护。同主机 Cookie 不按协议或端口隔离，不为兼容混用扩大 Domain 或取消 Secure。
- HTTP 可能暴露密码、会话与请求完整性。显式入口列表、窄代理 CIDR、同源和 CSRF 均不能替代 TLS 与防火墙；部署示例见 [README](../../README_CN.md)。
