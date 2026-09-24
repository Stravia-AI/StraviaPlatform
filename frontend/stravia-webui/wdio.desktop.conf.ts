// 本配置必须经 wdio bin 的 Node shim 运行（test:e2e:desktop 不得加 --bun）：
// Bun 1.4.x 对 expect@30 的 CJS/ESM 封装解析有误，AsymmetricMatcher 为
// undefined，expect-webdriverio 类继承会在加载期崩溃。
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
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
const startupFailure = process.env.STRAVIA_DESKTOP_E2E_STARTUP_FAILURE
if (startupFailure && startupFailure !== 'legacy' && startupFailure !== 'database') {
  throw new Error('STRAVIA_DESKTOP_E2E_STARTUP_FAILURE must be legacy or database')
}

const ownsRunRoot = !inheritedRootIsSafe
const runRoot =
  inheritedRootIsSafe && inheritedRunRoot ? resolve(inheritedRunRoot) : mkdtempSync(join(tmpdir(), runRootPrefix))
const codexHome = join(runRoot, 'codex')

try {
  mkdirSync(codexHome, { recursive: true })
  if (startupFailure && ownsRunRoot) {
    const fixtureDirectory = join(runRoot, 'data', ...(startupFailure === 'database' ? ['db'] : []))
    mkdirSync(fixtureDirectory, { recursive: true })
    writeFileSync(join(fixtureDirectory, 'gateway.db'), 'startup-recovery-fixture: preserve this data', { flag: 'wx' })
  }
} catch (error) {
  if (ownsRunRoot) {
    try {
      rmSync(runRoot, { recursive: true, force: true })
    } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], `Failed to initialize and clean up ${runRoot}`, {
        cause: cleanupError,
      })
    }
  }
  throw error
}

process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT = runRoot
process.env.CODEX_HOME = codexHome
// 恢复窗口不依赖业务数据根；测试仍将 WebView2 配置隔离到可清理的临时目录。
process.env.WEBVIEW2_USER_DATA_FOLDER = join(runRoot, 'webview')

export const config: WebdriverIO.Config = {
  runner: 'local',
  specs: [startupFailure ? './e2e/desktop-startup.smoke.ts' : './e2e/desktop.smoke.ts'],
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
        startTimeout: 180_000,
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
  // WDIO 在调用钩子前捕获 Mocha 的预算，钩子内部设置 timeout 无法覆盖该外层截止时间。
  mochaOpts: { ui: 'bdd', timeout: 180_000 },
  onComplete: async () => {
    // 继承的目录属于启动本次 WDIO 的进程；本进程仅清理自己创建的目录。
    if (!ownsRunRoot) return
    // WebView2 锁文件在应用退出后仍可能被 msedgewebview2.exe 短暂持有。
    for (let attempt = 1; ; attempt++) {
      try {
        rmSync(runRoot, { recursive: true, force: true })
        return
      } catch (error) {
        if (attempt >= 20) {
          throw new Error(`stravia-desktop-e2e: 无法清理临时目录 ${runRoot}`, { cause: error })
        }
        const { promise: sleep, resolve: finishSleep } = Promise.withResolvers<void>()
        setTimeout(finishSleep, 250)
        await sleep
      }
    }
  },
}
