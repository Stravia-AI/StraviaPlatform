import { fileURLToPath } from 'node:url'

const appBinaryPath = fileURLToPath(new URL('../../target/debug/stravia-desktop.exe', import.meta.url))

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
}
