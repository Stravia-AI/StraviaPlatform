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

interface DesktopAdminSession {
  access_token: string
}

async function expectProtectedStatus(port: number, accessToken: string): Promise<void> {
  const url = `http://127.0.0.1:${port}/api/v1/status`
  expect((await fetch(url)).status).toBe(401)
  expect((await fetch(url, { headers: { Authorization: `Bearer ${accessToken}` } })).status).toBe(200)
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
    const nativeSession = (await browser.tauri.execute(({ core }) =>
      core.invoke('get_admin_session'),
    )) as DesktopAdminSession
    expect(nativeSession).not.toHaveProperty('refresh_token')
    await expectProtectedStatus(portState.currentPort, nativeSession.access_token)
    await browser.refresh()
    const restoredSession = (await browser.tauri.execute(({ core }) =>
      core.invoke('get_admin_session'),
    )) as DesktopAdminSession
    await expectProtectedStatus(portState.currentPort, restoredSession.access_token)
    expect(
      await browser.execute(
        (accessToken) => Object.values(localStorage).some((value) => value.includes(accessToken)),
        nativeSession.access_token,
      ),
    ).toBe(false)

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
    await expect($('input[type="password"]')).not.toExist()
    await expect($('button=Sign out')).not.toExist()

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
      await expectProtectedStatus(nextPort, nativeSession.access_token)
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
    await expectProtectedStatus(replacementPort, nativeSession.access_token)

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

    await browser.tauri.execute(({ core }) => core.invoke('plugin:window|close', { label: 'main' }))
    await expectProtectedStatus(replacementPort, nativeSession.access_token)
    await browser.tauri.execute(({ core }) => {
      setTimeout(() => void core.invoke('plugin:process|exit', { code: 0 }), 0)
    })
    await browser.waitUntil(
      async () => {
        try {
          await fetch(`http://127.0.0.1:${replacementPort}/healthz`)
          return false
        } catch {
          return true
        }
      },
      { timeout: 10_000, timeoutMsg: 'application exit did not stop the native HTTP listener' },
    )
    // 原生退出已结束内嵌 WebDriver；写入真实实例，而非 @wdio/globals 的只读取代理。
    globalThis.browser.sessionId = ''
  })
})
