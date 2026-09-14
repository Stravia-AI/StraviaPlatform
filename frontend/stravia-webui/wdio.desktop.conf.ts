import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const appBinaryPath = fileURLToPath(new URL('../../target/debug/stravia-desktop.exe', import.meta.url))
const runRootPrefix = 'stravia-desktop-e2e-'
const inheritedRunRoot = process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT
const inheritedRelativePath = inheritedRunRoot ? relative(resolve(tmpdir()), resolve(inheritedRunRoot)) : ''
const inheritedRootIsSafe = Boolean(
  inheritedRunRoot &&
  inheritedRelativePath &&
  !isAbsolute(inheritedRelativePath) &&
  !inheritedRelativePath.startsWith('..') &&
  basename(inheritedRunRoot).startsWith(runRootPrefix),
)
const runRoot =
  inheritedRootIsSafe && inheritedRunRoot ? resolve(inheritedRunRoot) : mkdtempSync(join(tmpdir(), runRootPrefix))
const codexHome = join(runRoot, 'codex')

mkdirSync(codexHome, { recursive: true })
process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT = runRoot
process.env.CODEX_HOME = codexHome

export const config: WebdriverIO.Config = {
  runner: 'local',
  specs: ['./e2e/desktop.smoke.ts'],
  maxInstances: 1,
  services: [
    [
      '@wdio/tauri-service',
      {
        appBinaryPath,
        driverProvider: 'embedded',
        embeddedPort: 4445,
        autoDownloadEdgeDriver: true,
        autoInstallTauriDriver: true,
        captureBackendLogs: true,
        captureFrontendLogs: true,
        startTimeout: 60_000,
      },
    ],
  ],
  capabilities: [{ browserName: 'tauri', 'tauri:options': { application: appBinaryPath } }],
  framework: 'mocha',
  reporters: ['spec'],
  logLevel: 'warn',
  waitforTimeout: 10_000,
  connectionRetryTimeout: 90_000,
  connectionRetryCount: 1,
  mochaOpts: { ui: 'bdd', timeout: 60_000 },
  onComplete: async () => {
    // 托管数据根目录统一后 WebView2 的用户数据目录位于 runRoot 内，
    // 其锁文件在应用退出后仍被 msedgewebview2.exe 短暂持有。临时目录清理是
    // 尽力而为：短暂重试，耗尽后仅告警，不让已通过的用例被清理失败判为失败。
    for (let attempt = 1; ; attempt++) {
      try {
        rmSync(runRoot, { recursive: true, force: true })
        return
      } catch (error) {
        if (attempt >= 20) {
          console.warn(`stravia-desktop-e2e: 无法清理临时目录 ${runRoot}: ${error}`)
          return
        }
        const { promise: sleep, resolve: finishSleep } = Promise.withResolvers<void>()
        setTimeout(finishSleep, 250)
        await sleep
      }
    }
  },
}
