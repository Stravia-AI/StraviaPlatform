import { defineConfig, devices } from '@playwright/test'

const port = Number(process.env.PLAYWRIGHT_PORT ?? 4173)
const baseURL = `http://127.0.0.1:${port}`
const usePrebuiltWebUI = process.env.PLAYWRIGHT_USE_PREBUILT === '1'
const webServerCommand = usePrebuiltWebUI
  ? `bun -e "if (!(await Bun.file('dist/index.html').exists())) throw new Error('PLAYWRIGHT_USE_PREBUILT=1 requires dist/index.html from task build:server')" && `
  : 'bun run build && '

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: { baseURL, colorScheme: 'light', screenshot: 'only-on-failure', trace: 'retain-on-failure' },
  webServer: {
    // `vite preview` crashes under Bun 1.4.x (oven-sh/bun#40350); the dedicated
    // Bun.serve static server in scripts/preview-server.ts avoids that path.
    command: `${webServerCommand}bun scripts/preview-server.ts --host 127.0.0.1 --port ${port} --strictPort`,
    url: baseURL,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
})
