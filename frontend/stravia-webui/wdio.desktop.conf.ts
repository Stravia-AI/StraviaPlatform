import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'

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

const themeKey = `Software\\Stravia\\Tests\\${basename(runRoot)}`
process.env.STRAVIA_DESKTOP_E2E_THEME_KEY = themeKey
for (const [name, value] of [
  ['SystemUsesLightTheme', '1'],
  ['AppsUseLightTheme', '0'],
]) {
  execFileSync('reg.exe', ['add', `HKCU\\${themeKey}`, '/v', name, '/t', 'REG_DWORD', '/d', value, '/f'])
}

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
  onComplete: () => {
    execFileSync('reg.exe', ['delete', `HKCU\\${themeKey}`, '/f'])
    rmSync(runRoot, { recursive: true, force: true })
  },
}
