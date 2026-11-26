# 无 Windows 物理机环境下的 Windows 测试方案设计

在仅配备 Linux x64 宿主机或自托管 Forgejo Runner 的环境中，无法直接使用 GitHub Hosted `windows-2022` / `windows-11-arm` 节点。针对 Stravia 的桌面端与 Windows 交付面（`stravia-server.exe`、`stravia-desktop.exe`、NSIS 安装包、`windows-sys`/`windows` Win32 绑定、WebView2 桌面壳），采用**三层递进验证架构**，在无独立 Windows 宿主机的前提下最大化覆盖跨平台桌面行为与真实 Windows 专属行为，同时明确各层边界与取舍。

> 协议说明：桌面 E2E 使用 `@wdio/tauri-service` 的 `driverProvider: 'embedded'` 模式，由应用内置的 `tauri-plugin-wdio-webdriver` 直接监听 `127.0.0.1:4445` 提供 WebDriver 端点，不依赖外部 `msedgedriver.exe` 或 `tauri-driver`。

---

## 1. 测试面拆解

| 测试面 | 实际依赖 | 可否在 Linux 原生或 Wine 下运行 | 是否必须真实 Windows 内核/WebView2 |
|---|---|---|---|
| 跨平台 Tauri 桌面 E2E（IPC、启动恢复、插件导入/验签、代理路由、存储故障） | WebKitGTK + Xvfb + Openbox + D-Bus + `xclip` + 内置 `tauri-plugin-wdio-webdriver` | **可以**（Linux 原生直接运行全部 4 套桌面 E2E） | 否 |
| Windows 目标编译与链接 (`x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc`) | `cargo-xwin` + `clang-cl` + `rust-lld` + Windows SDK/CRT | 不需运行，Linux 静态交叉编译 | 否 |
| `stravia-server.exe` 启动、SQLite、TLS/AWS-LC、HTTP `/healthz` | Win32 文件/Socket/IOCP/Crypto API | **可以**（Wine 10+/11 对 Win32 Socket/SQLite/Rust std 支持成熟） | 否 |
| `stravia-desktop.exe --smoke-test-runtime`（内置 Wasmtime + SQLite + Core） | Win32 进程/内存页保护/Wasmtime JIT | **可以**（不初始化 WebView2） | 否 |
| Rust 单元测试 (`stravia-web-access`、`stravia-core`、`stravia-credential-protection` 等) | `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="$(command -v wine64 \|\| command -v wine)"` | **大部分可以**（纯逻辑、HTTP、SQLite、路径处理） | 否 |
| Windows WebView2 Desktop E2E（WebView2 渲染、Win32 托盘/窗口焦点/原生剪贴板、单实例锁） | 真实 WebView2 Runtime、Win32 消息循环、DWM、交互式桌面 Session 1 | **不可靠**（WebView2 在 Wine 下稳定性差） | **是** |
| NSIS 安装包 (`*-setup.exe`) 静默安装/卸载验证 | NSIS 脚本、注册表、快捷方式 | **部分可以**（Wine 下可跑 `/S` 静默安装检查文件释放与卸载器） | 完整 UI/UAC/SmartScreen 需真实 Windows |

---

## 2. 三层测试方案

### Layer 0：日常开发与每次 PR —— Linux 原生无头 Desktop E2E

利用 `tauri-plugin-wdio-webdriver` 的 Linux WebKitGTK 后端，在 Linux 宿主机直接运行完整桌面 E2E，无需 Windows 镜像或虚拟化。

- **无头封装**：`tests/common/run-desktop-e2e.ts` 在无 `DISPLAY`/`WAYLAND_DISPLAY` 时自动启用 `WEBKIT_DISABLE_DMABUF_RENDERER=1`、`dbus-run-session`、`xvfb-run -a -s "-screen 0 1920x1080x24"` 与 `openbox` 窗口管理器；Windows/macOS 零开销透传。
- **平台适配**：
  - `frontend/stravia-webui/wdio.desktop.conf.ts` 在 Linux 下为每次运行隔离 `XDG_DATA_HOME`/`XDG_CONFIG_HOME`/`XDG_CACHE_HOME`；`tests/e2e/storage/test_storage_failures.py` 同样对 Linux 桌面存储故障测试做了 XDG 隔离。
  - `frontend/stravia-webui/e2e/desktop.smoke.ts` 的原生剪贴板校验按平台选用 PowerShell `Get-Clipboard`（Windows）、`pbpaste`（macOS）或 `wl-paste`/`xclip`/`xsel`（Linux）。
  - NixOS 开发环境在 `devenv.nix` 中直接提供 `xvfb-run`、`xclip`、`openbox`，并将 `xz` 与 `libayatana-appindicator` 加入 Linux `LD_LIBRARY_PATH`（分别供 `@tauri-apps/cli` 动态链接 `liblzma.so.5` 与 `libappindicator-sys` 运行时 `dlopen` `libayatana-appindicator3.so.1`）。
- **入口命令**：`task test:e2e:desktop` 与 `task test:all`（`tests/common/run-all.ts` 已同时支持 Windows 与 Linux，覆盖 `desktop`、`desktop-legacy-recovery`、`desktop-database-recovery`、`desktop-storage-failures` 全部 4 套）。

---

### Layer 1：每次 PR/Push 必跑 —— Linux x64 交叉编译 + Wine 冒烟与单元测试

已集成在 `.github/workflows/ci.yml` 的 `windows-cross-smoke` job 与 `.github/scripts/wine_smoke_windows.sh` 中。

1. **MSVC ABI 交叉编译**：使用 `cargo-xwin`（而非 MinGW），保持与正式发布完全一致的 `x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc` 工具链、`rust-lld` 链接器与 Windows SDK 头文件，直接暴露 `#[cfg(target_os = "windows")]` 编译错误、Win32 API 签名错误与链接符号缺失。
2. **Wine 真实 PE 执行**：
   - 在隔离 `WINEPREFIX` 中启动 `stravia-server.exe`，轮询 `http://127.0.0.1:<port>/healthz`，验证 PE 加载、mimalloc 分配器、Tokio IOCP/AFD 网络层、SQLite 初始化与优雅退出。
   - 执行 `stravia-desktop.exe --smoke-test-runtime`，在不依赖 WebView2 的前提下验证桌面二进制的 Wasmtime 可执行内存与内置 Vendor 插件运行时。
   - 设置 `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="$(command -v wine64 || command -v wine)"`，运行 `cargo xwin test --target x86_64-pc-windows-msvc` 验证 Windows 路径语义、文件锁、环境变量与协议行为。

**优点**：纯容器、无需特权或 KVM、耗时低、适合每次 PR 门禁。

**局限**：不覆盖 WebView2 渲染、`Win32_UI_WindowsAndMessaging` 窗口/托盘交互及 Windows ARM64 运行时（Wine x86_64 只能执行 x64 PE）。

---

### Layer 2：按需/发版前 —— Linux x64 `/dev/kvm` 临时 Windows VM 跑完整 WebView2 Desktop E2E

利用 Linux x64 宿主机的硬件虚拟化（`/dev/kvm`），在不采购独立 Windows 物理机、不维护长期漂移 Windows Runner 的前提下，通过 **只读黄金镜像 + 临时 CoW Overlay + 外置依赖缓存** 跑完整的 Windows WebView2 Desktop E2E。

已提供：
- 黄金镜像制作脚本 `.github/scripts/build_windows_kvm_image.sh`；
- 执行脚本 `.github/scripts/kvm_windows_e2e.sh`；
- Nix 可复现构建与运行封装 `nix/win-vm/`（暴露 `packages.x86_64-linux.windows-e2e-image`、`windows-e2e-runner`、`apps.x86_64-linux.windows-e2e`）及 `devenv.nix` 按需接入；
- CI job `windows-kvm-desktop-e2e`（`runs-on: linux-x64-kvm`）。

#### 架构设计（两层正交解耦）

1. **OS 底座黄金镜像（`24G` 虚拟盘，与项目 lockfile 彻底解耦，数月不变）**：
   - 基于微软官方免费提供的 **Windows 10 Enterprise LTSC 2021 Evaluation ISO**（SeaBIOS 引导，无需 OVMF/TPM；`kvm_windows_e2e.sh` 亦通过 `WINDOWS_OVMF_CODE` / `WINDOWS_TPM=1` 兼容 Win11 镜像）。
   - 所有外部资产（Win10 LTSC ISO、固定版本 `virtio-win` ISO、**离线版 WebView2 Evergreen Standalone Installer**、Win32-OpenSSH `v9.5.0.0p1-Beta`、Node.js 22、Bun `1.4.2`、`uv`、`python-build-standalone` Windows x64 CPython 3.12）全部固定版本化 URL 与 SHA-256，可在断网环境（`OFFLINE_BUILD=1` / Nix `sandbox = true` + `kvm`）下 100% 离线构建。
   - **刻意不预热 `bun-cache` / `uv-cache`**：修改 `bun.lock` / `uv.lock` / `Cargo.lock` 永远不会导致 OS 镜像失效重建。
   - **零静态 SSH 密钥**：构建期仅使用临时密钥完成宿主机→Guest 公钥认证自检，封盘前自动删除镜像内 `authorized_keys`，改为在镜像中预装 `C:\tools\stravia-seed-sync.ps1`（`ONSTART` 任务），每次运行通过仅 ~360 KiB 的临时 `STRAVIA_SEED` 只读 ISO 动态注入一次性公钥。
   - 通过 `Autounattend.xml` + `provision.ps1` 自动完成：最小 VirtIO 驱动（`viostor` / `NetKVM` / `vioserial`）注入、1920x1080 桌面、`AutoAdminLogon` Session 1 自动登录、离线安装上述全部工具链、tiny11maker 风格二次裁剪（卸载完整 Edge 浏览器并严格保留 WebView2 Runtime、移除非必需 Capabilities 与高抖动服务）、`DISM /StartComponentCleanup /ResetBase`、`compact /CompactOS:always`、原生 `.NET FileStream` 空闲块清零与 `qemu-img convert -c -o compression_type=zstd` 压缩为只读基础盘。
2. **外置依赖缓存（宿主机按 lockfile 哈希持久化）**：
   - `kvm_windows_e2e.sh` 将 Windows 侧的 `bun-cache` 与 `uv-cache` 独立保存在宿主机 `WINDOWS_DEPS_CACHE_DIR`（默认 `~/.cache/stravia-win-vm/deps-cache/`），按 `bun.lock` / `uv.lock` 的 SHA-256 前 16 位索引（`bun-cache-<hash>.tar`、`uv-cache-<hash>.tar`）。
   - 首次在线运行（或执行 `WINDOWS_WARM_CACHE_ONLY=1`）自动在 Guest 内填充缓存并原子回传至宿主机；后续所有运行（含 `WINDOWS_OFFLINE=1` 断网模式）直接上传并解压复用，无需重建 OS 镜像。
3. **每次运行零污染启动 + Session 1 交互执行**：
   - Linux 宿主机先用 `cargo-xwin` 交叉编译 `stravia-desktop.exe`（`--config tauri.e2e.conf.json`）、`stravia-storage-test-harness.exe` 与 `target/vendor-test-fixtures/`（**不在 Windows VM 内编译 Rust**）。
   - `kvm_windows_e2e.sh` 用 `qemu-img create -f qcow2 -F qcow2 -b "$BASE_IMAGE_ABS" "$OVERLAY_IMG"` 创建毫秒级写时复制差分盘、生成一次性 ed25519 密钥与 `seed.iso` 并启动无头 QEMU/KVM；在 `WINDOWS_OFFLINE=1` 下若镜像同目录存在 `metadata.json`，默认将 QEMU `-rtc` 钉在镜像构建日，避免只读不可变 Eval 镜像超过 90 天后触发评估水印与 1 小时自动关机。
   - **规避 SSH Session 0 陷阱**：等待 `explorer.exe` 在 Session 1 就绪后，通过 `schtasks /Create ... /IT /RL HIGHEST` 将测试脚本投递到已登录的**交互式桌面 Session 1** 执行，实时流式回传日志与退出码，确保 WebView2 合成器、窗口焦点、系统托盘和原生剪贴板行为与真实用户桌面一致。
   - 默认完整覆盖 `desktop`、`legacy`、`database`、`storage-failures` 4 套测试（可通过 `WINDOWS_SUITES` 按需筛选）。结束时 `trap` 自动关闭 QEMU 并删除临时差分盘。

#### 本地执行 Windows KVM E2E（推荐：Nix / `devenv`）

前置条件：Linux x64 宿主机可读写 `/dev/kvm`，且 Nix daemon 已开启 `system-features = kvm`（`nix config show | grep system-features`）。

1. **在 Linux 宿主机准备 Windows 交叉编译产物与测试夹具**（不在 Windows VM 内编译 Rust）：
   ```bash
   task prepare:crates
   task build:vendor-fixtures
   task build:web:desktop-e2e

   # 交叉编译 stravia-desktop.exe（desktop-e2e）
   (cd backend/apps/stravia-desktop && bunx --bun @tauri-apps/cli@2.12.6 build \
     --ci --debug --no-bundle \
     --runner cargo-xwin \
     --target x86_64-pc-windows-msvc \
     --config tauri.e2e.conf.json -- --locked)

   # 若需跑 storage-failures 套件，另编 Windows 存储测试 harness
   cargo xwin build --locked -p stravia-server --bin stravia-storage-test-harness \
     --features stravia-server/test-harness --target x86_64-pc-windows-msvc
   ```

2. **在 `devenv shell` 中直接运行 Windows E2E**（默认按需惰性获取只读黄金镜像，不增大日常 `devenv shell` 体积）：
   ```bash
   # 运行全部 4 套（desktop,legacy,database,storage-failures）；首次联网会自动缓存外置依赖到 ~/.cache/stravia-win-vm/deps-cache/
   stravia-win-e2e

   # 仅跑桌面与启动恢复（不跑 storage-failures，约 2–3 分钟）
   WINDOWS_SUITES=desktop,legacy,database stravia-win-e2e

   # 已有宿主机外置依赖缓存后，全断网复跑（QEMU restrict=on，自动按 metadata.json 固定 RTC）
   WINDOWS_OFFLINE=1 stravia-win-e2e

   # 仅预热宿主机外置 bun-cache / uv-cache，不执行测试
   WINDOWS_WARM_CACHE_ONLY=1 stravia-win-e2e
   ```

3. **其他入口与配置方式**：
   - **独立预构建只读黄金镜像**：`nix build .#windows-e2e-image`
   - **通过 Flake App 直接运行**（无需进入 `devenv shell`）：`nix run .#windows-e2e`
   - **在本地 `devenv.local.nix` 中显式固化镜像并自动导出 `WINDOWS_BASE_IMAGE`**：
     ```nix
     {
       stravia.windowsVm.enable = true;
     }
     ```
   - **非 Nix 宿主机回退入口**：先执行 `bash .github/scripts/build_windows_kvm_image.sh` 生成只读基础盘，再设置 `WINDOWS_BASE_IMAGE` 后执行 `bash .github/scripts/kvm_windows_e2e.sh`（不再需要手工配置 `WINDOWS_SSH_KEY`）。
   - **注意**：`nix build .#windows-e2e-image` / 默认 `stravia-win-e2e` 使用 `flake.lock` 锁定的 `nixpkgs`，而 `stravia.windowsVm.enable = true` 使用 `devenv.lock` 锁定的 `nixpkgs`（若两边 `nixpkgs` 版本漂移，可能产生不同的 Nix store 哈希）；同一台机器或私有缓存建议统一入口，或通过 `stravia.windowsVm.package` 显式覆写为同一个 derivation。
   - **分发与合规提醒**：由于包含微软评估版 Windows 镜像，**严禁**将 `windows-e2e-image` 推送至公开 Cachix 或公开对象存储；团队 / CI 共享应仅使用内部私有 Nix Binary Cache（如 Attic / Harmonia）或自托管宿主机只读卷。

---

## 3. 方案对比与选型结论

| 方案 | 覆盖内容 | 耗时 | 运维复杂度 | 定位 |
|---|---|---|---|---|
| **Layer 0: Linux 原生无头 Desktop E2E** | 4 套全部跨平台桌面 E2E（WebKitGTK + 内置 WebDriver） | ~1–4 min（无 VM） | 极低（`devenv` 开箱即用） | **日常开发与每次 PR 门禁** |
| **Layer 1: `cargo-xwin` + Wine** | Windows MSVC 交叉编译 + PE/Server/Wasmtime/单测 | 3–8 min | 低（纯容器） | **每次 PR 必跑** |
| Wine + WebView2 | 不稳定（频繁崩溃） | 中 | 高 | **不推荐** |
| **Layer 2: Linux KVM + LTSC 只读镜像 Overlay** | **100% 真实 Windows Session 1 + WebView2 + Win32 托盘/剪贴板** | 3–8 min（仅跑测试不编译） | 中（脚本一键制作只读基础盘） | **Windows 专项回归与发版前验证** |
| 长期运行的物理 Windows Runner | 是 | 慢（含编译） | 高（状态漂移、维护成本高） | 无 Windows 机器时不可行 |
