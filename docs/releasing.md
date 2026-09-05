# 发布带签名的 Desktop 更新

Stravia Desktop 使用 Tauri updater 的独立签名密钥。GitHub Release 是更新发现和下载的唯一来源；发布流程要求 Windows x86_64、Windows ARM64、Linux x86_64、Linux ARM64 四个平台的 updater 产物与签名全部存在，才会生成 `stravia-updater.json` 并公开 Release。

## 首次生成和托管密钥

在受控的离线工作站生成密钥，不要在 CI runner、共享终端或仓库目录中生成：

```bash
bunx tauri signer generate -- -w /secure/offline/stravia-updater.key
```

将以下值写入 GitHub Actions repository secrets：

- `TAURI_SIGNING_PRIVATE_KEY`：私钥文件内容。
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：私钥密码；发布流程不接受空值。
- `STRAVIA_UPDATER_PUBLIC_KEY`：对应公钥文件的完整两行内容，或以 `RW` 开头的 key payload；workflow 会为单行 payload 补齐 Minisign comment。公钥不是秘密，但以 secret 注入可让 workflow 在构建前统一检查配置完整性。

至少保留一份与 GitHub 分离的离线加密私钥备份，并把密码存放在独立的受控凭据系统中。记录备份责任人、创建日期和恢复演练日期；不得把私钥、密码或其未脱敏命令输出写入仓库、日志、Actions artifact 或 Release。

## 发布

1. 确保 `Cargo.toml`、`package.json` 和 `backend/apps/stravia-desktop/tauri.conf.json` 版本一致，并在 `CHANGELOG.md` 中存在该版本的非空章节。
2. 从 `main` 中的目标 commit 创建 `vMAJOR.MINOR.PATCH` 或合法 SemVer prerelease tag。
3. 推送 tag。`release.yml` 会运行完整 CI，签署四个平台产物，执行 `.github/scripts/generate-updater-manifest.py`，创建 draft Release，发布容器与 Nix 产物，最后公开 Release。
4. 任一签名 secret、`.sig`、平台产物或清单字段缺失时 workflow 必须失败。不要通过移除校验、手工上传无签名安装包或发布部分清单来绕过失败。

`stravia-updater.json` 使用版本化 Release asset URL，并内联每个平台 `.sig` 的内容。普通安装包、`.sig`、清单和 `SHA256SUMS` 会一起上传到同一个精确版本 Release。

## 公钥轮换与恢复限制

不能用远端配置替换已安装应用的信任根。轮换时必须：

1. 保留旧私钥可用。
2. 构建一个同时承载迁移逻辑、并仍由旧私钥签名的过渡 Desktop 版本。
3. 确认受支持平台均已安装过渡版本后，才用新私钥签署后续版本。
4. 继续保管旧私钥，直到支持窗口结束。

旧私钥和所有离线备份都丢失后，已有安装无法通过 updater 恢复自动更新链。只能让用户手动安装带新公钥的完整安装包；不得禁用验签或接受旧版本远程下发的新公钥。

## 发布前人工 smoke

使用非生产签名的旧版本到新版本矩阵，在每个平台记录检查、下载、验签、安装、重启和重启后版本：

- Windows x86_64：确认仅出现 NSIS passive 原生进度窗。
- Windows ARM64：确认仅出现 NSIS passive 原生进度窗。
- Linux x86_64 AppImage：确认应用内安装进度、原地替换和 relaunch。
- Linux ARM64 AppImage：确认应用内安装进度、原地替换和 relaunch。

同时验证签名损坏、平台条目缺失、只读 AppImage 和网络失败都 fail closed。没有对应架构设备时，在发布记录中明确写明未验证的平台、原因和风险，不能把另一架构的结果当作替代。
