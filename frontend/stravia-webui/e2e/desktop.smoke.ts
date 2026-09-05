import { createServer } from 'node:net'
import type { AddressInfo } from 'node:net'

import { $, browser, expect } from '@wdio/globals'

interface DesktopPortState {
  currentPort: number
  fixedPort: number | null
  mode: 'fixed' | 'fallback' | 'configError'
}

interface DesktopUpdateState {
  phase: 'idle' | 'downloading' | 'downloaded' | 'installing' | 'error'
}

async function unusedPort(): Promise<number> {
  const server = createServer()
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  const port = (server.address() as AddressInfo).port
  await new Promise<void>((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()))
  })
  return port
}

describe('Stravia desktop smoke', () => {
  it('boots the native shell with the WebDriver bridge', async () => {
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    const serverPort = await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))
    const portState = (await browser.tauri.execute(({ core }) =>
      core.invoke('get_desktop_port_state'),
    )) as DesktopPortState
    expect(serverPort).toEqual(expect.any(Number))
    expect(portState.currentPort).toEqual(serverPort)
    expect((await fetch(`http://127.0.0.1:${serverPort}/api/v1/status`)).ok).toBe(true)

    const brand = await $('[aria-label="Stravia 观策行"]')
    await expect(brand).toBeDisplayed()

    const navigationTrigger = await $('header button[aria-expanded]')
    await expect(navigationTrigger).toBeDisplayed()

    if (portState.mode === 'fallback') {
      await expect($('//h2[normalize-space()="Fixed desktop port unavailable"]')).toBeDisplayed()
      await $('a=Resolve in Desktop Settings').click()
    } else if (portState.mode === 'configError') {
      await expect($('//h2[normalize-space()="Desktop port setting unavailable"]')).toBeDisplayed()
      await $('a=Open Desktop Settings').click()
    } else {
      await browser.execute(() => setTimeout(() => window.location.assign('/settings#desktop'), 0))
    }

    await expect($('//h2[normalize-space()="Local access"]')).toBeDisplayed()
    await expect($('#desktop-fixed-port')).toHaveValue(String(portState.fixedPort ?? portState.currentPort))
    expect((await $('#desktop').getAttribute('class')).split(/\s+/)).toContain('route-section')

    let activePort = portState.currentPort
    if (portState.mode !== 'fixed') {
      const nextPort = await unusedPort()
      await $('#desktop-fixed-port').setValue(String(nextPort))
      await $('button=Save Fixed Port').click()
      await browser.waitUntil(
        async () => {
          try {
            const state = (await browser.tauri.execute(({ core }) =>
              core.invoke('get_desktop_port_state'),
            )) as DesktopPortState
            return state.mode === 'fixed' && state.currentPort === nextPort
          } catch {
            return false
          }
        },
        { timeout: 10_000, timeoutMsg: 'desktop listener did not hot-switch to the saved port' },
      )
      activePort = nextPort
      await expect($('[aria-label="Stravia 观策行"]')).toBeDisplayed()
      expect((await fetch(`http://127.0.0.1:${nextPort}/api/v1/status`)).ok).toBe(true)
    }

    let replacementPort = await unusedPort()
    if (replacementPort === activePort) replacementPort = await unusedPort()
    await $('#desktop-fixed-port').setValue(String(replacementPort))
    await $('button=Save Fixed Port').click()
    const confirmation = await $('[data-slot="alert-dialog-title"]')
    await expect(confirmation).toHaveText('Change fixed desktop port?')
    await expect($('[role="alertdialog"]')).toHaveText(expect.stringContaining(`127.0.0.1:${activePort}`))
    await $('button=Change Port').click()
    await browser.waitUntil(
      async () => {
        try {
          const state = (await browser.tauri.execute(({ core }) =>
            core.invoke('get_desktop_port_state'),
          )) as DesktopPortState
          return state.mode === 'fixed' && state.currentPort === replacementPort
        } catch {
          return false
        }
      },
      { timeout: 10_000, timeoutMsg: 'confirmed desktop listener switch did not complete' },
    )
    expect((await fetch(`http://127.0.0.1:${replacementPort}/api/v1/status`)).ok).toBe(true)

    await expect($('//h2[normalize-space()="Updates"]')).toBeDisplayed()
    await expect($('button=Download update')).toBeDisplayed()
    await $('button=Download update').click()
    await expect($('[role="progressbar"]')).toHaveAttribute('aria-valuenow', '50')
    await expect($('button=Pause')).not.toExist()
    await expect($('button=Cancel')).not.toExist()
    await expect($('[data-slot="dialog-title"]')).toHaveText('Install Stravia 9.9.9?')
    await expect($('[data-slot="dialog-description"]')).toHaveText(
      'Stravia will exit now. Any Gateway requests in progress will be interrupted.',
    )
    await (await $('[data-slot="dialog-content"]')).$('button=Close').click()
    await expect($('[data-slot="dialog-title"]')).not.toExist()
    await $('button=Exit and install').click()
    await expect($('[data-slot="dialog-title"]')).toHaveText('Install Stravia 9.9.9?')
    await (await $('[data-slot="dialog-content"]')).$('button=Exit and install').click()
    await browser.waitUntil(
      async () => {
        const state = (await browser.tauri.execute(({ core }) =>
          core.invoke('get_desktop_update_state'),
        )) as DesktopUpdateState
        return state.phase === 'installing'
      },
      { timeout: 10_000, timeoutMsg: 'desktop updater did not enter the installing state' },
    )
    await expect($('p=Installing Stravia 9.9.9…')).not.toBeDisplayed()
  })
})
