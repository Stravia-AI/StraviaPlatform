# 管理用户与首次初始化设计

状态：已实施。本文记录当前管理认证、首次设置与部署迁移契约。

## 身份与权限

- 用户系统仅替换管理面的静态 Admin Token。模型调用与 MCP 继续使用现有 API Key 和 Principal，不建立 API Key 到管理用户的归属关系。
- 每个独立实例只有一个管理员，唯一角色为 `admin`，拥有全部管理权限。不提供其他用户的创建、禁用或删除功能。
- Server 管理员通过用户名和密码登录，可退出登录及修改自身凭据。不提供公开注册、邮箱验证或邮件找回。
- 旧 Admin Token 不再作为管理登录凭据，不保留双认证兼容路径。

## JWT 与管理会话

- 访问 JWT 有效期为 15 分钟；使用可撤销、轮换的 refresh token 静默续期，登录总有效期最多为 7 天。
- JWT 验签之外还必须校验服务端会话状态。退出撤销当前会话，修改密码或本地恢复凭据撤销全部旧会话；后续请求与刷新均不得继续使用已撤销会话。
- 认证存储故障返回 `503`，不能被当成无效凭据或成功注销。撤销未完成时不得清除 Cookie；WebUI 保留当前页面并显示错误，避免把仍有效的服务端会话误报为已退出。
- Server WebUI 通过 `stravia_access`（`Path=/`）与 `stravia_refresh`（`Path=/api/v1/auth`）两个 `HttpOnly`、`SameSite=Strict` Cookie 携带凭据；HTTPS origin 下同时设置 `Secure`。前端不将凭据存入 localStorage。
- `GET /api/v1/auth/state` 报告 `setup`、`server`、`desktop` 或 `unavailable` 模式，以及当前请求的认证/设置资格；该只读检查不会刷新凭据。`POST /api/v1/auth/login`、`POST /api/v1/auth/refresh`、`POST /api/v1/auth/logout` 与 `PUT /api/v1/auth/credentials` 是 Server 日常认证入口；响应正文不返回 token。所有会修改状态的 Web 请求均要求 `Origin` 精确等于规范 origin，并携带 `X-Stravia-CSRF: 1`；需要正文时还必须使用 JSON content type。
- Desktop 通过受限原生通道取得 access JWT，仅在内存中保存，以 Bearer JWT 请求 HTTP 管理 API；refresh token 只保留在原生进程内存中。两种载体共享后端会话验证。登录总有效期耗尽后，Desktop 可通过原生通道重新取得会话。
- 取舍见 [ADR-0040](../adr/0040-separate-admin-identity-and-revoke-sessions.md)。

## Desktop

- 首次启动直接使用本地 SQLite，自动创建唯一管理员，无初始化向导；后续启动复用该身份并自动登录。
- 自动登录只授予原生应用。普通浏览器或其他本机进程不能仅凭访问回环地址获得管理权限，HTTP 管理 API 不再免认证。
- Desktop 仅支持原生登录，不提供用户名或密码设置，不生成默认密码。若将该数据库交给 Server 使用，必须通过本地恢复命令设置用户名和密码；Desktop 本身不因此开放密码登录。
- Desktop 不提供独立的“退出登录”。实际退出应用时结束当前原生会话，下次启动自动登录；关闭窗口到托盘不算退出应用。

## Server 首次设置

### 进入条件

- 配置文件负责定位数据库，目标数据库是否已有管理员是初始化状态的唯一事实源，不另存本地 `initialized` 标志。
- 全新安装尚未选择数据库时，先进入受设置令牌保护的数据库选择流程。
- 已配置的数据库可正常读取且无管理员时，进入设置流程；已有管理员时仅开放正常登录。
- 配置损坏或已配置数据库连接失败必须报错，不视为全新安装，不自动创建替代数据库或重新开放设置。
- 初始化完成前只提供初始化所需服务，不继续提供模型调用或 MCP。

### 一次性设置令牌

- 未完成初始化的 Server 启动时在控制台打印设置令牌。`POST /api/v1/setup/claim` 首次验证成功即消费令牌，并发领取最多一个成功；它换取 `Path=/api/v1`、`SameSite=Strict` 的 `stravia_setup` HttpOnly 设置 Cookie（HTTPS origin 下同时设置 `Secure`），只能访问 `POST /api/v1/setup/test` 与 `POST /api/v1/setup/complete`，不能调用正常管理 API。
- 设置令牌和初始化会话在当前进程内不限时。数据库连接测试失败可在该会话内修正后重新提交。
- 未完成初始化时重启 Server 会打印新令牌，旧令牌与旧初始化会话均失效；初始化完成后关闭设置权限。
- 已接受风险：未消费的令牌若进入持久化控制台日志，在该进程存活且未完成初始化期间持续有效。

### 数据库选择与配置

- SQLite 自动创建本地数据库文件。PostgreSQL 连接用户事先创建的数据库，验证连接后运行 Stravia 自身的 schema migrations；不创建 PostgreSQL 数据库，不要求 `CREATEDB` 权限，也不安装或启动 PostgreSQL 服务。
- 数据库连接以 `server.toml` 配置文件为唯一来源；`--config <path>` 显式选择文件，默认路径是 `<data-dir>/server.toml`。`--data-dir` 仅定位运行时产物，不覆盖数据库。旧数据库 CLI 参数与环境变量入口已删除，不保留覆盖或兼容读取路径。
- 配置使用带 `backend` tag 的 `[database]`：SQLite 写入 `backend = "sqlite"` 与以 `gateway.db` 结尾的 `path`；PostgreSQL 写入 `backend = "postgres"`、`url`，并可选 `max_connections`、`min_connections`、`idle_timeout_seconds`。PostgreSQL URL 属于秘密，配置文件需受文件权限保护。
- SQLite 相对路径以配置文件所在目录为基准，连接测试、初始化、启动和本地恢复使用同一解析规则；向导保存解析后的绝对路径。默认 Debug 配置和 SQLite 数据库分别位于当前 workspace 的 `.stravia-dev/server.toml` 与 `.stravia-dev/gateway.db`，不随进程工作目录改变。
- 若向导选择的数据库已有管理员，保存连接配置后关闭设置权限，转到正常登录页；必须使用该数据库已有管理员的凭据，设置令牌不能重建或覆盖管理员。
- 数据库配置来源取舍见 [ADR-0041](../adr/0041-own-database-connection-in-config-file.md)。

### 提交与恢复

1. 验证数据库连接及配置目录可写。
2. 原子保存连接配置。
3. 运行所需迁移，并在数据库事务中创建唯一管理员。
4. 关闭初始化入口，在同一进程启用正常 Gateway，无需手动重启。
5. 浏览器转到登录页，使用刚设置的用户名和密码登录；设置会话不直接升级为管理会话。

管理员创建失败时保留连接配置，允许修正；重启后按目标数据库有无管理员恢复流程。文件与数据库不构成跨系统原子事务，不通过删除数据库或管理员补偿失败。账户已创建但页面未收到成功响应时，不能再次覆盖账户，应恢复为正常登录。

## 已有安装与凭据恢复

- 当前仓库支持的 schema 通过本次增量迁移加入用户与会话结构，保留 Provider、API Key、历史及其他已有业务数据。不补齐所有历史 schema 的升级链；更旧或不兼容的 schema 明确报错，不删库重建。
- 已有 Server 首次升级通过新的设置令牌创建管理员，完成前只提供初始化服务。已有 PostgreSQL 安装必须先将原连接配置迁入文件，不能因缺少配置默认切换到 SQLite。
- Server 提供 `stravia-server --config <path> recover-admin` 本地交互式凭据恢复命令；操作者针对同一数据库运行它。命令在终端读取用户名，并以无回显方式读取新密码及确认，原地更新唯一管理员并撤销全部旧会话。
- 恢复要求配置文件、目标数据库与既有管理员均可读；它不删除管理员、不重新开放数据库选择向导、不提供邮件找回。密码不得通过命令行参数或日志传递。
- 非回环 `--host` 必须同时配置 HTTPS `--public-origin`；远程部署应由反向代理终止 TLS，不信任转发 header 推导规范 origin。Docker 与 Nix 的数据库连接同样只来自持久化的 `server.toml`。

## 实施验收

- 管理 JWT 不能作为推理或 MCP 的 API Key；原有调用身份、权限与数据归属保持不变。
- 单管理员约束能抵御并发初始化请求；设置令牌只能成功消费一次，旧设置权限不能覆盖已存在的管理员。
- 退出、修改密码与本地恢复后的旧 JWT 及 refresh token 均不能通过后续认证；过期会话不可刷新。
- Web 刷新页面可恢复有效登录；轮换刷新能正确处理同一浏览器的并发请求，不能因正常并发意外锁死会话。
- Desktop 首次启动自动创建身份，重复启动不重复创建；原生应用可自动登录，普通回环 HTTP 请求不能免认证。
- SQLite 与 PostgreSQL 均覆盖首次创建、当前受支持 schema 增量迁移、已有管理员库接入、连接失败及初始化中断恢复。
- 配置写入失败不创建管理员；管理员创建失败保留配置；管理员已提交后重启进入正常登录，不重复创建或重置。
- 已配置数据库故障不触发新的首次设置；不兼容 schema 不被清空或重建。
- Server 初始化成功后无需重启即可使用正常服务；本地凭据恢复不影响业务数据。
- 同步更新 README 两种语言、部署入口、数据库 schema 文档与生成的 PostgreSQL 参考 schema，并验证 WebUI、Server 和实际 Desktop 表面。

## 实施状态

该设计已完成干净切换：静态 Admin Token 与数据库 CLI/环境变量入口不再受支持。部署者必须按本文迁移 `server.toml`；API Key 与 Principal 的推理/MCP 契约不受影响。
