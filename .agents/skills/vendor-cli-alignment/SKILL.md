---
name: vendor-cli-alignment
description: 专属 Vendor 插件对齐官方 CLI 新版本、隔离抓包（TLS ClientHello / HTTP / WebSocket）、更新 egress shape 或捕获夹具，以及排查上游门控与协议漂移时使用。
---

# 专属 Vendor 插件抓包与版本对齐

每次对齐官方 CLI 新版本或排查上游门控／协议漂移，都按「基线取证 → 隔离抓包 → 同步实现与形态 → 重建验证与清理」推进。宿主网络与 Egress Shape 的设计边界见 `docs/design/vendor-plugins.md`，不在本文件重复。

## 1. 建立仓库基线与上游差异

先从仓库 live 文件确立现状，不凭记忆或旧文档假设版本：

- 读取目标插件 crate（`backend/crates/stravia-vendor-<crate>/src/` 与 `tests/`）、对应 `shape/data/*.json`、`shape/fixtures/*`、宿主验收测试与 `docs/research/` 文档，列出当前对齐版本、UA、端点、头序、传输封装、请求体白名单、shape 名称及 `revision`。
- 核对官方最新稳定版变更：
  - **开源客户端**：对比当前对齐 tag 至最新版源码 diff，重点检查版本常量、UA 组合、模型发现与版本门控、HTTP／WS 头序与 beta 标志、请求体白名单、工具声明与会话元数据。
  - **闭源二进制**：下载并按官方校验和验真安装包，结合反汇编／符号／protobuf 描述符确认常量、枚举、UA 与传输路径；下载包与 updater manifest 来源 URL 不同时分别校验，不凭版本号推定字节一致。
- 组合字段（如 UA 中的 SDK／runtime／CL 版本、内部枚举值、实验开关）一律以官方源码或真实二进制抓取为准，逐项核实后再动代码。

完成条件：每项协议、门控与模型差异均映射到具体源码位置、二进制证据或待抓包验证点；区分实证事实、单变量推论与未证实项。

## 2. 隔离抓包与最小上游验证

在独立临时目录 `/tmp/<vendor>-align-<ver>/` 内执行官方客户端，使用隔离的 `HOME`／`XDG_*`／`APPDATA`、空配置与空工作区、进程级临时 CA；关闭分享并限制工具权限为只读或 `ask`，避免污染宿主环境或触发真实工具执行。

- **仅需离线观测 ClientHello 与请求头序**：优先使用 `stravia-tools capture-client`（在 loopback 记录同连接 ClientHello 与脱敏 HTTP／WS 原始头，参数见 `--help`）。
- **需过真实上游（如多步 bootstrap、动态元数据或验证 HTTP 200 接受）**：使用本地 loopback TLS MITM 代理捕获同连接原始 ClientHello 字节、HTTP/1.1 或 H2／WS 头序及脱敏请求体；仅对最小代表样本转发官方端点，其余用例本地合成响应终止。
- **覆盖完整传输面**：同一客户端存在多套传输栈或连接模式时（如 HTTP 与 WebSocket、一元 RPC 与流式推理），分别抓取；每条路径至少采集两次独立连接（对应 `*.json` 与 `*.repeat.json`）以识别每连接变化字段（随机扩展序、session ticket、padding、nonce、request-id）。
- **凭据与脱敏边界（硬约束）**：
  - 匿名免费层以最小请求探活并留意公网 IP 限流，不触碰个人账号。
  - 若任务需要借用线上 Stravia OAuth／账号凭据，先确认本次请求已获明确授权；仅以只读方式（SQLite 使用 `file:...?mode=ro`）在内存中提取短期 `access_token`，原库保持只读且不读取、不复制、不轮换 `refresh_token`。
  - 所有抓包产物程序化脱敏后方可进入仓库：`Authorization`、`Host`、动态 `Content-Length`、cookie、account／session／request ID 替换为 `<redacted>`，保留真实头序与静态字面量；所有凭据与真实对话内容不打印、不入库、不写入宿主 HOME。

完成条件：取得每条受影响传输栈的同连接 ClientHello 与脱敏请求捕获，并完成脱敏校验；原库与宿主环境无未授权改动。

## 3. 对齐插件代码、Egress Shape、夹具与文档

- **插件实现**：按实测与源码证据同步版本常量、UA、端点、头序、帧封装（如 `Content-Length` 与 `Transfer-Encoding: chunked`）、请求体投影、工具分组、元数据标签及模型目录／兼容修复；出站 wire 适配留在协议边界，不污染 canonical 对话历史，不留旧版本特判或兼容垫片。
- **Egress Shape 与 `revision` 规则**（由 `shape/tests.rs` 强校验）：
  - 逐项比对新旧 ClientHello 的 cipher suites、extensions 及顺序／随机性、supported_groups、key_shares、signature_algorithms、ALPN、padding 与 JA3／JA4。
  - **TLS 或传输身份字节有变化**：必须递增 `shape/data/<name>.json` 的 `revision`，或按运行时切换／新增独立 shape（如内嵌 Bun 升级时核对现有 `bun*.json`），禁止复用已变更连接身份的旧 `revision`。
  - **仅更新采集元数据（`source` provenance）且 TLS 字节完全一致**：保持 `revision` 不变。
- **测试夹具与回归**：
  - 刷新对应 `shape/fixtures/*.json` 与 `*.repeat.json`；有对照价值的历史 CLI 请求样本可按版本保留。
  - 同步更新插件单元测试、真实 Wasm 契约测试与宿主 shape 验收测试，断言精准覆盖变更点（头序、帧封装、标签、工具形状与关键回归）。
- **文档与变更日志**：更新对应 `docs/research/*.md`（写明版本、采集日期、实连与合成样本边界，不把客户端抓包写成上游永久放行承诺）和 `CHANGELOG.md`（`## Unreleased`）。

完成条件：代码、shape、夹具、测试与文档口径一致，每处改动均有第 1–2 步证据对应。

## 4. 重建 Wasm、定向验证与清理

在最后一次编辑完成后执行验证，并确认每条测试命令实际运行且通过数大于 0：

1. 重建独立 Wasm 组件（`--only` 传 `stravia-build-vendors.rs` 中的 `vendor_id`，如 `antigravity`、`openai-codex`、`opencode-free`，产物写入 `target/vendor-plugins-all/`）：
   ```bash
   cargo run --locked -p stravia-vendor-base --bin stravia-build-vendors -- --only=<vendor-id>
   ```
2. 运行目标插件单元测试与真实 Wasm 契约测试：
   ```bash
   cargo test --locked -p stravia-vendor-<crate> --lib
   cargo test --locked -p stravia-vendor-<crate> --test <contract-test> -- --ignored
   ```
3. 运行宿主 shape、transport 与真实 Wasm 验收测试（注意验收模块通过 `#[path = ...]` 挂在 `gateway::vendor_auxiliary_tests` 下，须按挂载模块名或具体测试名过滤，避免按文件名过滤跑出 0 用例）：
   ```bash
   cargo test --locked -p stravia-core --lib plugin::network::shape::tests
   cargo test --locked -p stravia-core --lib <transport-or-acceptance-filter>
   ```
   若修改了插件自有 `plugin:<name>` shape，另运行 `stravia-tools verify-egress-shape` 离线核对。
4. 运行静态检查：`cargo clippy --locked -p stravia-vendor-<crate> --all-targets -- -D warnings` 与 `cargo fmt --check`。
5. **清理与密钥扫描**：删除 `/tmp/<vendor>-align-*` 与工作区内任何临时捕获目录；扫描全部新增和修改文件，确认无 OAuth token、refresh token、JWT、cookie、account ID 或密钥字面量残留。

完成条件：上述检查均在最后一次编辑后通过且无漏跑；临时目录已清、密钥扫描零命中；交付中明确报告版本差异、改动文件、实跑验证命令与结果、真实上游验证边界、剩余风险，以及部署时是否需要重新导入重建后的独立 Wasm 插件或重同步模型目录。未经明确授权不执行 `git commit` 或 `git push`。

## 实现与工具入口

- 宿主 Egress Shape 与插件边界：`docs/design/vendor-plugins.md`
- 抓包与离线形态校验 CLI：`backend/crates/stravia-devtools/src/capture_client/`、`backend/crates/stravia-devtools/src/verify_egress_shape.rs`（运行 `stravia-tools capture-client --help` / `verify-egress-shape --help`）
- Shape 定义、夹具与修订不变量测试：`backend/crates/stravia-core/src/plugin/network/shape/{data/,fixtures/,tests.rs}`
- 宿主真实 Wasm 验收测试挂载入口：`backend/crates/stravia-core/src/gateway/vendor_auxiliary_tests.rs`
- 独立 Vendor 构建与 `vendor_id` 清单：`backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs`
