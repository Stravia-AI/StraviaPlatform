# 无 Windows 物理机环境下的 Windows 测试方案设计

在仅配备 Linux x64 自托管 Forgejo Runner 的环境中，无法直接使用 GitHub Hosted `windows-2022` / `windows-11-arm` 节点。针对 Stravia 的 Windows 交付面（`stravia-server.exe`、`stravia-desktop.exe`、NSIS 安装包、`windows-sys`/`windows` Win32 绑定、WebView2 桌面壳），采用**三层递进验证架构**，在无独立 Windows 宿主机的前提下最大化覆盖真实 Windows 行为，同时明确各层边界与取舍。

---

## 1. Windows 测试面拆解

| 测试面 | 实际依赖 | 能否在 Wine 下运行 | 是否必须真实 Windows 内核/WebView2 |
|---|---|---|---|
| Windows 目标编译与链接 (`x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc`) | `cargo-xwin` + `clang-cl` + `rust-lld` + Windows SDK/CRT | 不需运行，Linux 静态交叉编译 | 否 |
| `stravia-server.exe` 启动、SQLite、TLS/AWS-LC、HTTP `/healthz` | Win32 文件/Socket/IOCP/Crypto API | **可以**（Wine 10+/11 对 Win32 Socket/SQLite/Rust std 支持成熟） | 否 |
| `stravia-desktop.exe --smoke-test-runtime`（内置 Wasmtime + SQLite + Core） | Win32 进程/内存页保护/Wasmtime JIT | **可以**（不初始化 WebView2） | 否 |
| Rust 单元测试 (`stravia-web-access`、`stravia-core`、`stravia-credential-protection` 等) | `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="$(command -v wine64 \|\| command -v wine)"` | **大部分可以**（纯逻辑、HTTP、SQLite、路径处理） | 否 |
| Tauri Desktop E2E (`wdio.desktop.conf.ts` + `msedgedriver` + WebView2) | 真实 WebView2 Runtime、`msedgedriver.exe`、Win32 窗口消息循环、DWM/托盘 | **不可靠**（WebView2 Evergreen/Fixed 在 Wine 下崩溃率高，且 `msedgedriver` 无法稳定挂载） | **是** |
| NSIS 安装包 (`*-setup.exe`) 静默安装/卸载验证 | NSIS 脚本、注册表、快捷方式 | **部分可以**（Wine 下可跑 `/S` 静默安装检查文件释放与卸载器） | 完整 UI/UAC/SmartScreen 需真实 Windows |

---

## 2. 三层测试方案

### Layer 1：每次 PR/Push 必跑 —— Linux x64 交叉编译 + Wine 冒烟与单元测试

已集成在 `.github/workflows/ci.yml` 的 `windows-cross-smoke` job 与 `.github/scripts/wine_smoke_windows.sh` 中。

1. **MSVC ABI 交叉编译**：使用 `cargo-xwin`（而非 MinGW），保持与正式发布完全一致的 `x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc` 工具链、`rust-lld` 链接器与 Windows SDK 头文件，直接暴露 `#[cfg(target_os = "windows")]` 编译错误、Win32 API 签名错误与链接符号缺失。
2. **Wine 真实 PE 执行**：
   - 在隔离 `WINEPREFIX` 中启动 `stravia-server.exe`，轮询 `http://127.0.0.1:<port>/healthz`，验证 PE 加载、mimalloc 分配器、Tokio IOCP/AFD 网络层、SQLite 初始化与优雅退出。
   - 执行 `stravia-desktop.exe --smoke-test-runtime`，在不依赖 WebView2 的前提下验证桌面二进制的 Wasmtime 可执行内存与内置 Vendor 插件运行时。
   - 设置 `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="$(command -v wine64 || command -v wine)"`（Ubuntu 24.04 的 `wine64` 包将可执行包装器提供为 `wine`），运行 `cargo xwin test --target x86_64-pc-windows-msvc` 验证 Windows 路径语义、文件锁、环境变量与协议行为。
3. **NSIS 安装包结构与 Wine 静默安装验证（发布前可选）**：
   - 用 Linux 原生 `makensis` 生成 `*-setup.exe` 后，在临时 `WINEPREFIX` 执行 `wine installer-setup.exe /S`，断言目标目录释放了 `stravia-desktop.exe` 与卸载程序。

**优点**：纯容器、无需特权或 KVM、耗时低（增量几分钟）、适合每次 PR 门禁。  
**局限**：不覆盖 WebView2 渲染、`msedgedriver`、`Win32_UI_WindowsAndMessaging` 窗口/托盘交互及 Windows ARM64 运行时（Wine x86_64 只能执行 x64 PE）。

---

### Layer 2：按需/发版前 —— Linux x64 `/dev/kvm` 临时 Windows VM 跑完整 WebView2 Desktop E2E

利用 Linux x64 Runner 自身的硬件虚拟化（`/dev/kvm`），在不采购独立 Windows 物理机、不维护长期漂移 Windows Runner 的前提下，通过 **只读黄金镜像 + 临时 CoW Overlay** 跑完整的 `wdio.desktop.conf.ts`。

已提供执行脚本 `.github/scripts/kvm_windows_e2e.sh` 与可选 job `windows-kvm-desktop-e2e`（`runs-on: linux-x64-kvm`）。

#### 架构设计

1. **离线制作只读黄金镜像（一次性）**：
   - 使用微软官方免费提供的 **Windows 11 Enterprise Evaluation / Windows Server 2022 Evaluation ISO**（或通过 [`dockur/windows`](https://github.com/dockur/windows) 自动化无人值守安装，并导出 `qcow2`）。
   - 预装：Microsoft Edge WebView2 Runtime、OpenSSH Server（公钥免密登录）、Node.js 22+、Bun `1.4.2`。
   - 关机后设为只读文件（如 `/var/lib/forgejo-runner/images/win11-e2e-base.qcow2`）。
2. **每次 CI 运行零污染启动**：
   - Linux 宿主机先用 `cargo-xwin` 交叉编译 `stravia-desktop.exe`（`--config tauri.e2e.conf.json`）和 `target/vendor-test-fixtures/`（**不在 Windows VM 内编译 Rust**，避免 VM 内编译慢且消耗磁盘）。
   - 脚本执行 `qemu-img create -f qcow2 -F qcow2 -b "$WINDOWS_BASE_IMAGE" "$OVERLAY_IMG"` 创建毫秒级写时复制差分盘。
   - 启动无头 `qemu-system-x86_64 -enable-kvm`（端口转发 `127.0.0.1:<ephemeral> -> 22`），等待 SSH 就绪（约 20–40 秒）。
   - 通过 `scp` 传入打包好的 `stravia-desktop.exe`、fixtures 与 `frontend/stravia-webui`，在 Guest 内执行 `bun run test:e2e:desktop`（完整覆盖 `prepareDesktopDriver()` + `msedgedriver` + WebView2 + WDIO 断言）。
   - 测试结束 `trap` 自动关闭 QEMU 并删除差分盘，黄金镜像永久洁净、可复现。
3. **触发策略**：
   - 默认不阻塞日常 PR（避免并发 KVM 内存压力）；通过仓库变量 `ENABLE_WINDOWS_KVM_E2E=true` 在 `main` / 发版 tag 开启，或通过 `workflow_dispatch` 的 `run_windows_kvm_e2e` 手动触发。

**资源需求**：宿主机需支持 `/dev/kvm`，单并发预留 4 vCPU + 8 GiB RAM + 40 GiB 只读基础镜像空间。

---

### Layer 3：架构演进建议 —— 将通用桌面 E2E 与 Windows 专属 E2E 解耦

当前 `frontend/stravia-webui/e2e/desktop-driver.ts` 在首行直接拒绝非 `win32` 平台（`if (process.platform !== 'win32') throw ...`），导致所有桌面端 E2E（包括与操作系统无关的 Tauri IPC、插件市场安装、Updater 验签、启动失败恢复页面）都绑定在 Windows 上。

建议后续拆分为：
- **Linux 原生 Tauri Smoke（每次 PR，无需 KVM）**：在 Linux x64 容器内用 `xvfb-run` + `tauri-driver` + `WebKitWebDriver` 跑跨平台 Tauri IPC / 插件 / 恢复流程测试。
- **Windows KVM Smoke（合入 `main` / 发版）**：只保留 Windows 专属差异验证（WebView2 宿主集成、Win32 单实例锁、托盘菜单、NSIS 安装器）。

---

## 3. 方案对比与选型结论

| 方案 | 覆盖 WebView2 / WDIO | CI 耗时 | 运维复杂度 | 是否推荐 |
|---|---|---|---|---|
| 仅 `cargo check --target ...` | 否 | 极快 | 极低 | 不足（漏链接与运行时错误） |
| **Layer 1: `cargo-xwin` + Wine** | 否（覆盖 PE/Server/Wasmtime/单测） | 3–8 min | 低（纯 Docker） | **推荐：每次 PR 必跑** |
| Wine + WebView2 | 不稳定（频繁崩溃） | 中 | 高 | **不推荐** |
| **Layer 2: Linux KVM + Overlay Windows VM** | **是（100% 真实 Windows + WebView2）** | 5–10 min（仅跑测试不编译） | 中（一次性准备黄金镜像） | **推荐：发版/主干按需运行** |
| 长期运行的物理 Windows Runner | 是 | 慢（含编译） | 高（状态漂移、维护成本高） | 无 Windows 机器时不可行 |
