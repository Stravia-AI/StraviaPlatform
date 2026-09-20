import { readFile } from 'node:fs/promises'
import { join } from 'node:path'

import { $, browser, expect } from '@wdio/globals'

interface StartupState {
  status: 'starting' | 'ready' | 'failed'
  stage: string
  error: { code: string; details: string } | null
  logPath: string | null
}

describe('Desktop startup recovery', () => {
  it('retains a usable recovery window without opening or replacing failed storage', async () => {
    await browser.tauri.switchWindow('main')
    const surface = $('[data-testid="desktop-startup"]')
    await expect(surface).toHaveAttribute('data-status', 'failed')
    await expect(surface).toBeDisplayed()
    const state = (await browser.tauri.execute(({ core }) => core.invoke('get_desktop_startup_state'))) as StartupState
    expect(state.status).toBe('failed')
    expect(state.error?.code).toBe(
      process.env.STRAVIA_DESKTOP_E2E_STARTUP_FAILURE === 'legacy' ? 'data_directory' : 'gateway',
    )
    await expect($('a[href="/providers"]')).not.toExist()
    expect(new URL(await browser.getUrl()).pathname).not.toBe('/login')
    await expect($('[data-testid="startup-restart"]')).toBeEnabled()
    await expect($('[data-testid="startup-exit"]')).toBeEnabled()
    await surface.$('summary').click()
    await expect($('[data-testid="startup-copy"]')).toBeEnabled()
    await $('[data-testid="startup-copy"]').click()

    const runRoot = process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT
    if (!runRoot) throw new Error('Missing isolated desktop test root')
    const fixture = join(
      runRoot,
      'data',
      ...(process.env.STRAVIA_DESKTOP_E2E_STARTUP_FAILURE === 'database' ? ['db'] : []),
      'gateway.db',
    )
    expect(await readFile(fixture, 'utf8')).toBe('startup-recovery-fixture: preserve this data')
    expect(state.logPath).not.toBeNull()
    const diagnostic = await readFile(state.logPath!, 'utf8')
    expect(diagnostic).toContain(state.error!.code)
    await browser.refresh()
    await expect($('[data-testid="desktop-startup"]')).toHaveAttribute('data-status', 'failed')
    await expect($('[data-testid="startup-restart"]')).toBeEnabled()
  })
})
