# 发布带签名的 Desktop 更新

Stravia Desktop 使用 Tauri updater 的独立签名密钥。GitHub Release 仍是更新产物、清单与签名的权威来源；应用更新发现默认通过内置的生产更新分发服务进行，也可用 `STRAVIA_UPDATER_URL` 覆盖，该服务提供 GitHub 兼容的 Release 列表与重写后的清单，`release_url` 仍指向官方 GitHub Release。发布流程要求 Windows x86_64、Windows ARM64、Linux x86_64、Linux ARM64 四个平台的 updater 产物与签名全部存在，才会生成 `stravia-updater.json` 并公开 Release。

Server 压缩包发布 Linux x86_64/ARM64 GNU 和 Windows x86_64/ARM64 版本；不再提供 Linux musl 静态二进制包。容器镜像仍使用 Debian 基础镜像。

容器 WebUI 依赖安装层必须同时复制 workspace manifests、`bun.lock` 和 `patches/`，再运行锁定安装；补丁变化必须使该层缓存失效。不得因补丁仅用于桌面测试而从容器安装输入或 `patchedDependencies` 中移除它。Rust 编译层还必须保留 `frontend/stravia-webui/src/assets/icons/` 的原始 SVG（供 Vendor Plugins 编译时内嵌品牌图标）和 `frontend/stravia-webui/static/stravia-logo.svg`（供 Gateway 与插件管理器内嵌）；WebUI 的 `dist` 不能替代这些源文件。

## 更新分发服务配置

Server 与 Desktop 内置生产更新分发服务 `https://updater.stravia.cn`，发布包无需额外设置环境变量即可使用。可用进程环境变量 `STRAVIA_UPDATER_URL` 覆盖；Server 的 `--updater-url` 优先于该环境变量。覆盖值必须为公开 HTTPS 基址，可带路径前缀，末尾斜杠会被规范化。两端由 Core 统一选择默认地址，不依赖 CI 注入或运行机器的环境配置。

- 更新检查请求 `<base>/api/v1/releases?per_page=100&page=N`，清单来自服务返回的 `stravia-updater.json` asset URL。客户端最多检查 10 页，超过视为错误。应用不再直接查询 GitHub Releases API，也没有旧入口后备。
- 未配置覆盖值时使用内置生产地址；显式空值、空白或非法地址只在检查时产生 `UPDATE_SOURCE_INVALID`，不会静默回退或阻止 Server/Desktop 启动及其他功能；debug 构建仍默认禁用生产更新检查。
- 发布通道、跳过版本、常规成功/失败检查 TTL、出站代理偏好、HTTPS 校验和 Tauri 签名校验保持不变。
- 历史缓存的 manifest 下载入口必须匹配当前生效的更新源，包括未覆盖时的内置生产源。旧 GitHub 地址、其他服务地址或非法覆盖配置下，仍展示历史版本信息但禁用下载并报告错误；不匹配来源的缓存不会阻止自动检查重新发现清单，也不会通过猜测 tag 改写旧地址。
- 服务返回的 `release_notes_url`/`html_url` 必须是官方 GitHub Release URL；清单平台 URL 由服务改写，但签名仍由现有 Tauri 公钥验证。

## 首次生成和托管密钥

在受控的离线工作站生成密钥，不要在 CI runner、共享终端或仓库目录中生成：

```bash
bunx --bun tauri signer generate -- -w /secure/offline/stravia-updater.key
```

将以下值写入 GitHub Actions repository secrets：

- `TAURI_SIGNING_PRIVATE_KEY`：私钥文件内容。
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：私钥密码；发布流程不接受空值。
- `STRAVIA_UPDATER_PUBLIC_KEY`：对应公钥文件的完整内容、以 `RW` 开头的 key payload，或公钥文件的 base64 编码；workflow 会规范化为 Tauri 所需的 base64 编码 Minisign 公钥文件。公钥不是秘密，但以 secret 注入可让 workflow 在构建前统一检查配置完整性。

至少保留一份与 GitHub 分离的离线加密私钥备份，并把密码存放在独立的受控凭据系统中。记录备份责任人、创建日期和恢复演练日期；不得把私钥、密码或其未脱敏命令输出写入仓库、日志、Actions artifact 或 Release。

## 发布

1. 确保 `Cargo.toml`、`package.json` 和 `backend/apps/stravia-desktop/tauri.conf.json` 版本一致，并在 `CHANGELOG.md` 中存在该版本的非空章节。
2. 从 `main` 中的目标 commit 创建 `vMAJOR.MINOR.PATCH` 或合法 SemVer prerelease tag。
3. 推送 tag。`release.yml` 会确认完整 CI 通过，签署四个平台产物，执行 `.github/scripts/generate_updater_manifest.py`，创建 draft Release，发布容器镜像，最后公开 Release。
4. 任一签名 secret、`.sig`、平台产物或清单字段缺失时 workflow 必须失败。不要通过移除校验、手工上传无签名安装包或发布部分清单来绕过失败。

发布回归必须遵循当前产品契约：已开始的 Responses 流只以单个 `response.failed` 收口，并保留部分输出和原生错误详情；重启恢复用例必须在上游请求仍受事件门控、尚未交付时模拟进程中断，不能以固定延迟或优雅关闭后的已完成请求代替崩溃场景。修复失败后先运行对应定向检查，再重新执行完整发布矩阵。

CI 的 Admin 与 Chromium E2E 复用 `task test:e2e:admin` 和 `task test:e2e:web`，不直接以生产 `target/release/stravia-server` 替代测试服务。Admin 先串行准备 `build:test:development` 与 `build:test:services`；Chromium 先串行准备 `build:test:release` 与 `build:test:services`。`target/test-artifacts/services/` 保留带 `test-harness` 的独立服务副本，支持 loopback catalog 等测试参数；生产副本与开发模式副本独立保存，后续不同 feature graph 的构建不能覆盖正在验证的测试服务。

`stravia-updater.json` 使用版本化 Release asset URL，并内联每个平台 `.sig` 的内容。普通安装包、`.sig`、清单和 `SHA256SUMS` 会一起上传到同一个精确版本 Release。更新分发服务读取该清单后仅把平台下载 URL 重写为自己的下载端点；版本、平台键、签名和下载字节保持不变。

### 同一提交的 CI 复用

`dev`、`main` push 与版本 tag 仍各自触发工作流，但 `ci.yml` 在启动重型检查前通过 `.github/scripts/reuse_ci.py` 查询同仓库、精确相同 SHA 的自动完整 CI。已有成功运行时直接复用；已有排队或执行中的运行时等待其成功，没有合格候选时照常运行完整检查。只有 `main` / `dev` 的 `push` CI 可以作为证据；其他 SHA、其他仓库、PR、手动运行、缓存预热、失败、取消或跳过的运行均不能作为成功证据。

独立 CI 只等待运行 ID 更小的 CI，Release 只等待独立 CI，不互相等待。API 失败、等待超时、候选运行失败或被取消都会使守门任务失败，不会把未验证的提交报告为通过。PR 始终验证自己的合并结果；显式 `workflow_dispatch`（包括缓存预热）始终执行其选择的任务。并发组按调用工作流、事件、ref 和预热模式隔离，避免手动预热或分支 CI 取消 Release 检查。

查询运行状态只需要 `actions: read`，不授予取消运行或其他 Actions 写权限。Actions 页面仍可能出现多个轻量守门/等待条目，其 summary 会链接被复用的 CI；不再为已确认的同一提交重复启动整套测试。Rust 缓存仍只由现有可信 `main` writer 写入；当 `main` 复用 `dev` CI 时不会额外写缓存，需要时可在 `main` 手动选择 `cache_warm`。`Docker cache` 负责独立预热镜像构建缓存，不是完整 CI，不参与此复用规则。

## 公钥轮换与恢复限制

不能用远端配置替换已安装应用的信任根。轮换时必须：

1. 保留旧私钥可用。
2. 构建一个同时承载迁移逻辑、并仍由旧私钥签名的过渡 Desktop 版本。
3. 确认受支持平台均已安装过渡版本后，才用新私钥签署后续版本。
4. 继续保管旧私钥，直到支持窗口结束。

旧私钥和所有离线备份都丢失后，已有安装无法通过 updater 恢复自动更新链。只能让用户手动安装带新公钥的完整安装包；不得禁用验签或接受旧版本远程下发的新公钥。

## 发布前人工 smoke

使用非生产签名的旧版本到新版本矩阵，在每个平台记录检查、下载、验签、安装、重启和重启后版本：

- Windows x86_64：确认仅出现 NSIS passive 原生进度窗。已保存安装语言时不得额外出现语言选择窗。
- Windows ARM64：确认仅出现 NSIS passive 原生进度窗。已保存安装语言时不得额外出现语言选择窗。
- Linux x86_64 AppImage：确认应用内安装进度、原地替换和 relaunch。
- Linux ARM64 AppImage：确认应用内安装进度、原地替换和 relaunch。

Smoke 环境必须把 `STRAVIA_UPDATER_URL` 指向受控更新分发服务，避免未覆盖时访问生产源；显式空值或非法覆盖必须明确失败而不是静默回退。同时验证签名损坏、平台条目缺失、只读 AppImage 和网络失败都 fail closed。没有对应架构设备时，在发布记录中明确写明未验证的平台、原因和风险，不能把另一架构的结果当作替代。
