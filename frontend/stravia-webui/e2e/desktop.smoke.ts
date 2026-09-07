import { createServer } from 'node:net'
import { execFileSync } from 'node:child_process'
import type { AddressInfo } from 'node:net'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { basename, isAbsolute, join, relative, resolve } from 'node:path'

import { $, browser, expect } from '@wdio/globals'

interface DesktopPortState {
  currentPort: number
  fixedPort: number | null
  mode: 'fixed' | 'fallback' | 'configError'
}

interface DesktopUpdateState {
  phase: 'idle' | 'downloading' | 'downloaded' | 'installing' | 'error'
}

interface CreatedResource {
  id: string
}

interface CreatedRoute extends CreatedResource {
  model_id: string
}

async function adminRequest(serverPort: number, path: string, init?: RequestInit): Promise<unknown> {
  const session = (await browser.tauri.execute(({ core }) => core.invoke('get_admin_session'))) as DesktopAdminSession
  const response = await fetch(`http://127.0.0.1:${serverPort}/api/v1${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${session.access_token}`,
      ...(init?.body ? { 'content-type': 'application/json' } : {}),
    },
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${init?.method ?? 'GET'} ${path} failed (${response.status}): ${text}`)
  if (!text) return undefined
  const payload: unknown = JSON.parse(text)
  if (typeof payload !== 'object' || payload === null) {
    throw new Error(`${init?.method ?? 'GET'} ${path} returned an invalid response`)
  }
  return 'data' in payload ? payload.data : undefined
}

function createdResource(value: unknown, label: string): CreatedResource {
  if (typeof value !== 'object' || value === null || !('id' in value) || typeof value.id !== 'string') {
    throw new Error(`${label} response did not include an id`)
  }
  return { id: value.id }
}

function createdRoute(value: unknown): CreatedRoute {
  const resource = createdResource(value, 'Route')
  if (typeof value !== 'object' || value === null || !('model_id' in value) || typeof value.model_id !== 'string') {
    throw new Error('Route response did not include a model_id')
  }
  return { ...resource, model_id: value.model_id }
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
      await expect($('aria/Fixed desktop port unavailable')).toBeDisplayed()
      await $('a=Resolve in Desktop Settings').click()
    } else if (portState.mode === 'configError') {
      await expect($('aria/Desktop port setting unavailable')).toBeDisplayed()
      await $('a=Open Desktop Settings').click()
    } else {
      await $('a[href="/settings"]').click()
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
  })

  it('writes Codex global configuration incrementally from the actual Connect page', async () => {
    const runRoot = process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT
    const codexHome = process.env.CODEX_HOME
    if (!runRoot || !codexHome) throw new Error('Desktop smoke isolation directories were not configured')
    const relativeCodexHome = relative(resolve(runRoot), resolve(codexHome))
    if (
      !relativeCodexHome ||
      isAbsolute(relativeCodexHome) ||
      relativeCodexHome.startsWith('..') ||
      basename(runRoot).startsWith('stravia-desktop-e2e-') === false
    ) {
      throw new Error(`Refusing to run Connect Apply outside the isolated desktop smoke directory: ${codexHome}`)
    }

    await mkdir(codexHome, { recursive: true })
    const configPath = join(codexHome, 'config.toml')
    const catalogPath = join(codexHome, 'stravia-models.json')
    await writeFile(
      configPath,
      [
        'model = "user-current-model"',
        'approval_policy = "never"',
        '',
        '[model_providers.existing]',
        'name = "Existing provider"',
        'base_url = "https://existing.invalid/v1"',
        '',
        '[profiles.personal]',
        'model = "profile-current-model"',
        '',
      ].join('\n'),
      'utf8',
    )

    const serverPort = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const fixtureSuffix = basename(runRoot).slice('stravia-desktop-e2e-'.length)
    const modelId = `desktop-smoke-${fixtureSuffix}`
    const keyName = `Desktop smoke key ${fixtureSuffix}`
    let provider: CreatedResource | undefined
    let route: CreatedRoute | undefined
    let apiKey: CreatedResource | undefined
    const failures: unknown[] = []

    try {
      provider = createdResource(
        await adminRequest(serverPort, '/providers', {
          method: 'POST',
          body: JSON.stringify({
            name: `Desktop smoke provider ${fixtureSuffix}`,
            source: { type: 'custom', protocol: 'open-responses', base_url: 'https://desktop-smoke.invalid' },
            credential: { type: 'none' },
            use_proxy: false,
          }),
        }),
        'Provider',
      )
      await adminRequest(serverPort, `/providers/${provider.id}/models`, {
        method: 'POST',
        body: JSON.stringify({ model_id: modelId, metadata: { id: modelId, name: 'Desktop Smoke Model' } }),
      })
      route = createdRoute(
        await adminRequest(serverPort, '/models/bind', {
          method: 'POST',
          body: JSON.stringify({ provider_id: provider.id, provider_model_id: modelId }),
        }),
      )
      apiKey = createdResource(
        await adminRequest(serverPort, '/api-keys', {
          method: 'POST',
          body: JSON.stringify({ key: `sk-desktop-smoke-${fixtureSuffix}`, name: keyName, model_ids: [route.id] }),
        }),
        'API Key',
      )

      // 管理 API 在页面外准备夹具，重新加载以免复用前一个 smoke 的配置查询缓存。
      await browser.refresh()
      await browser.tauri.switchWindow('main')
      await $('a[href="/connect"]').click()
      await expect($('//h1[normalize-space()="Connect clients"]')).toBeDisplayed()
      // WebDriver 桥的合成 click 不产生 Select 所需的 pointer 事件，使用其键盘交互。
      await $('#cli-key').click()
      await browser.keys('ArrowDown')
      await expect($(`//*[@role="option" and contains(normalize-space(), "${keyName}")]`)).toBeDisplayed()
      await browser.keys(keyName)
      await browser.keys('Enter')
      await expect($('#cli-key')).toHaveText(expect.stringContaining(keyName))

      const writeConfiguration = await $('button=Write configuration')
      const copyConfiguration = await $('button=Copy')
      await expect(writeConfiguration).toBeDisplayed()
      await expect(writeConfiguration).toBeEnabled()
      await expect(copyConfiguration).toBeDisplayed()
      await expect(copyConfiguration).toBeEnabled()
      const incrementalPreview = await $('pre.route-code-plane')
      await expect(incrementalPreview).toBeDisplayed()
      await expect(incrementalPreview).toHaveText(expect.stringContaining('model_provider = "stravia"'))
      await expect(incrementalPreview).not.toHaveText(expect.stringContaining('user-current-model'))
      await expect(incrementalPreview).not.toHaveText(expect.stringContaining('approval_policy'))

      // 原生剪贴板拒绝未聚焦的文档。
      await browser.execute(() => window.focus())
      await copyConfiguration.click()
      await expect($('//*[@data-sonner-toast and contains(., "Copied to clipboard")]')).toBeDisplayed()

      await writeConfiguration.click()
      await expect($('//*[@data-sonner-toast and contains(., "Configuration written for Codex")]')).toBeDisplayed()

      // WDIO 运行于 Node，借用已有 Bun 解析 TOML，不固定序列化器的引号格式。
      const writtenConfig: unknown = JSON.parse(
        execFileSync('bun', ['-e', 'process.stdout.write(JSON.stringify(Bun.TOML.parse(await Bun.stdin.text())))'], {
          input: await readFile(configPath, 'utf8'),
          encoding: 'utf8',
        }),
      )
      expect(writtenConfig).toMatchObject({
        model: 'user-current-model',
        approval_policy: 'never',
        model_provider: 'stravia',
        model_catalog_json: catalogPath,
        model_providers: {
          existing: { name: 'Existing provider', base_url: 'https://existing.invalid/v1' },
          stravia: expect.any(Object),
        },
        profiles: { personal: { model: 'profile-current-model' } },
      })

      const catalogPayload: unknown = JSON.parse(await readFile(catalogPath, 'utf8'))
      if (typeof catalogPayload !== 'object' || catalogPayload === null || !('models' in catalogPayload)) {
        throw new Error('Codex model catalog did not contain a models collection')
      }
      if (!Array.isArray(catalogPayload.models)) throw new Error('Codex model catalog models value was not an array')
      const writtenModel = catalogPayload.models.find(
        (candidate) =>
          typeof candidate === 'object' && candidate !== null && 'slug' in candidate && candidate.slug === modelId,
      )
      expect(writtenModel).toBeDefined()
    } catch (error) {
      failures.push(error)
    }
    const resources = [
      apiKey && `/api-keys/${apiKey.id}`,
      route && `/models/${encodeURIComponent(route.model_id)}`,
      provider && `/providers/${provider.id}`,
    ].filter((path): path is string => Boolean(path))
    for (const path of resources) {
      try {
        await adminRequest(serverPort, path, { method: 'DELETE' })
      } catch (error) {
        failures.push(error)
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Desktop Connect smoke and fixture cleanup failed')
  })

  it('captures a rejected request through native Request Records and clears retained diagnostics', async () => {
    const serverPort = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    await $('a[href="/logs"]').click()
    await expect(browser).toHaveUrl(expect.stringContaining('/logs'))
    const debugSwitch = await $('[role="switch"][aria-label="Debug"]')
    await expect(debugSwitch).toBeEnabled()
    await expect(debugSwitch).toHaveAttribute('aria-checked', 'false')
    await debugSwitch.click()
    await expect($('[role="alertdialog"]')).toBeDisplayed()
    await (await $('[role="alertdialog"]')).$('button=Enable Debug').click()
    await expect(debugSwitch).toHaveAttribute('aria-checked', 'true')

    const rejected = await fetch(`http://127.0.0.1:${serverPort}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'desktop-observation', messages: [{ role: 'user', content: 'local smoke' }] }),
    })
    expect(rejected.status).toBe(401)
    await $('button=Rejected Requests').click()
    const request = await $('.rejection-list button')
    await expect(request).toHaveText(expect.stringContaining('POST /v1/chat/completions'))
    await expect(request).toHaveText(expect.stringContaining('HTTP 401'))
    await request.click()
    const inspector = await $('[aria-label="Observation details"]')
    await expect(inspector).toBeDisplayed()
    await inspector.$('button=Debug records').click()
    await expect(inspector.$('.debug-record pre')).toHaveText(
      expect.stringContaining('"direction": "client_to_platform"'),
    )
    await debugSwitch.click()
    await expect(debugSwitch).toHaveAttribute('aria-checked', 'false')

    await $('button=Clear history').click()
    await (await $('[role="alertdialog"]')).$('button=Clear history').click()
    await expect($('.rejection-list')).not.toExist()
  })

  // 退出会关闭共享的原生会话，必须在所有页面交互验证之后执行。
  it('keeps the session in the tray and stops the listener on application exit', async () => {
    const serverPort = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const nativeSession = (await browser.tauri.execute(({ core }) =>
      core.invoke('get_admin_session'),
    )) as DesktopAdminSession
    await browser.tauri.execute(({ core }) => core.invoke('plugin:window|close', { label: 'main' }))
    await expectProtectedStatus(serverPort, nativeSession.access_token)
    await browser.tauri.execute(({ core }) => {
      setTimeout(() => void core.invoke('plugin:process|exit', { code: 0 }), 0)
    })
    await browser.waitUntil(
      async () => {
        try {
          await fetch(`http://127.0.0.1:${serverPort}/healthz`)
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
