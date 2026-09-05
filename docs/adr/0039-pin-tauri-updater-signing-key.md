---
status: accepted
---

# 固定 Stravia Desktop 更新签名密钥

Stravia Desktop 使用 Tauri updater 的强制签名校验：安装包内嵌专用 updater 公钥，Release workflow 从 GitHub Actions secrets 读取私钥及密码签署更新产物，并为私钥保留至少一份受控的离线加密备份。只把私钥留在 GitHub 会让 secret 丢失永久切断已安装版本的自动更新链；引入外部签名服务则为当前发布规模增加不必要的运行依赖。

## Consequences

- updater 私钥不得进入仓库、构建日志或 Release 资产。
- 轮换公钥前必须先用旧私钥发布一个内嵌新公钥的过渡版本；旧私钥丢失后不能原地恢复已有安装的自动更新能力。
- Release workflow 缺少签名 secret 时必须失败，不得发布无签名或跳过校验的 Desktop 更新清单。
