---
name: releasing
description: 发布 Stravia 正式版或 beta 预发布版、决定版本号与 Git tag、准备 CHANGELOG 及核对 GitHub Release 状态时使用。
---

执行或评审 Stravia 版本发布时的操作流程。签名密钥、插件目录、macOS 验证与发布前 smoke 细节见 `docs/releasing.md`，不在本文件重复。

## 1. 确定版本号

所有发布 tag 格式为 `v<version>`，`<version>` 必须符合 SemVer 2.0 且不含 `+build` 元数据（由 `.github/workflows/release.yml` 强制校验）。

### Beta 版本号规则（极简单阶段递增）

- 格式固定为 `X.Y.Z-beta.N`（如 `0.8.0-beta.1` → `0.8.0-beta.2`），点分隔纯数字序号且无前导零（保证 `semver::Version` 按数值排序：`beta.2 < beta.10 < X.Y.Z`）。
- 以仓库已发布的版本为准，按 SemVer 比较最新正式 release 与最新 beta：
  1. **最新已发布版本是 beta**，或**最新 beta 的 SemVer 高于最新正式 release**（例如最新正式版 `0.7.7`、已有 `0.8.0-beta.2`）→ 继续递增该 beta 序号（`0.8.0-beta.3`）。
  2. **否则**（最新正式 release 已大于等于所有已有 beta，或尚无 beta）→ **必须显式指明新的目标 beta 版本号**（如 `0.7.8-beta.1` 或 `0.8.0-beta.1`），不自行猜测下一正式版是 minor 还是 patch。

完成标准：目标版本号唯一确定，且严格高于同通道已有版本。

## 2. 同步仓库版本与变更日志

1. 将以下三处版本号更新为完全一致的 `<version>`：
   - `Cargo.toml`（`[workspace.package].version`）
   - `package.json`
   - `backend/apps/stravia-desktop/tauri.conf.json`
2. 在 `CHANGELOG.md` 中添加 `## [<version>]` 非空章节（`.github/scripts/release_api.py` 会直接提取该章节作为 Release Notes）。

完成标准：三处版本号与目标 `<version>` 逐字一致，且 `CHANGELOG.md` 对应章节非空。

## 3. 创建与推送 Tag（需用户明确授权）

- 仅从 `origin/main` 祖先提交创建 `v<version>` tag 并推送，触发 `.github/workflows/release.yml`。
- 未经用户明确授权，不执行 `git push` 或发布操作。

## 4. 核对 GitHub Release

- **Beta 预发布版**：必须在 GitHub Release 上标记为 **Pre-release**（`release.yml` 会对带 `-prerelease` 的 tag 自动传入 `--prerelease true`，并仅推送精确版本容器镜像，不覆盖 `latest` 或主次版本标签）。如手工调整或补发 Release，必须确保勾选 `Pre-release`。
- **正式版**：确认未标记 `Pre-release`，且六平台 updater 产物、`.sig`、`stravia-updater.json`、`SHA256SUMS` 均已上传并完整。

完成标准：GitHub Release 的 `Pre-release` 标记、容器标签与资产清单均与目标版本类型一致。
