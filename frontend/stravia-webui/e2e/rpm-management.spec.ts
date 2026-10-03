import { expect, test as base, type Page } from '@playwright/test'
import { spawn, type ChildProcess } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, rm } from 'node:fs/promises'
import { createServer, request, type Server } from 'node:http'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { stripVTControlCharacters } from 'node:util'

import type { ApiKey, Provider, ProviderModelDetail, Route } from '../src/lib/types'
import type { RpmConfig } from '../src/lib/rpm'

const root = resolve(import.meta.dirname, '../../..')
const binary = resolve(
  process.env.STRAVIA_BINARY ??
    join(root, 'target/release', process.platform === 'win32' ? 'stravia-server.exe' : 'stravia-server'),
)
const password = 'correct horse battery staple'

async function api<T>(page: Page, path: string, method = 'GET', body?: unknown): Promise<T> {
  const result = await page.evaluate(
    async ({ path, method, body }) => {
      const response = await fetch(`/api/v1${path}`, {
        method,
        headers: { 'content-type': 'application/json', 'x-stravia-csrf': '1' },
        body: body === undefined ? undefined : JSON.stringify(body),
      })
      const text = await response.text()
      return { status: response.status, text }
    },
    { path, method, body },
  )
  expect(result.status, `${method} ${path}: ${result.text}`).toBeGreaterThanOrEqual(200)
  expect(result.status, `${method} ${path}: ${result.text}`).toBeLessThan(300)
  return result.text ? (JSON.parse(result.text).data as T) : (undefined as T)
}

async function stopProxy(server: Server): Promise<void> {
  if (!server.listening) return
  const closed = new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())))
  server.closeAllConnections()
  await closed
}

const test = base.extend<{ management: { origin: string; setupToken: string } }>({
  management: [
    async ({ baseURL }, use) => {
      if (!baseURL || new URL(baseURL).hostname !== '127.0.0.1')
        throw new Error('A local Playwright preview baseURL is required')
      const preview = new URL(baseURL)
      const directory = await mkdtemp(join(tmpdir(), 'stravia-rpm-browser-'))
      let backendPort = 0
      let backend: ChildProcess | undefined
      let backendClosed: Promise<void> | undefined
      // Debug binaries have no embedded UI. Only management APIs go to the real
      // backend; static assets and SPA navigation use Playwright's existing preview.
      const proxy = createServer((incoming, outgoing) => {
        if (!incoming.url?.startsWith('/') || incoming.url.startsWith('//')) {
          outgoing.writeHead(403).end()
          return
        }
        if (incoming.url.startsWith('/local-upstream')) {
          outgoing
            .writeHead(200, { 'content-type': 'application/json' })
            .end(JSON.stringify({ object: 'list', data: [{ id: 'rpm-upstream', object: 'model', owned_by: 'local' }] }))
          return
        }
        const backendRequest = /^\/(api|v1|v1beta)(\/|\?|$)/.test(incoming.url)
        const port = backendRequest ? backendPort : Number(preview.port)
        const headers = { ...incoming.headers, host: `127.0.0.1:${port}` }
        delete headers.forwarded
        delete headers['x-forwarded-for']
        headers['x-forwarded-host'] = incoming.headers.host
        headers['x-forwarded-proto'] = 'http'
        const upstream = request(
          { hostname: '127.0.0.1', port, path: incoming.url, method: incoming.method, headers },
          (response) => {
            outgoing.writeHead(response.statusCode ?? 502, response.headers)
            response.pipe(outgoing)
          },
        )
        upstream.on('error', () => {
          if (outgoing.headersSent) outgoing.destroy()
          else outgoing.writeHead(502).end()
        })
        outgoing.on('close', () => upstream.destroy())
        incoming.pipe(upstream)
      })
      // Backend background catalog/update traffic must never reach production.
      proxy.on('connect', (_request, socket) => socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'))
      let failure: unknown
      let failed = false
      const cleanupErrors: unknown[] = []
      try {
        proxy.listen(0, '127.0.0.1')
        await once(proxy, 'listening')
        const address = proxy.address()
        if (!address || typeof address === 'string') throw new Error('Expected TCP listener')
        const origin = `http://127.0.0.1:${address.port}`
        backend = spawn(
          binary,
          [
            '--host',
            '127.0.0.1',
            '--port',
            '0',
            '--log-level',
            'info',
            '--data-dir',
            directory,
            '--trusted-proxy',
            '127.0.0.1',
            '--admin-origin',
            origin,
          ],
          {
            cwd: directory,
            stdio: ['ignore', 'pipe', 'pipe'],
            env: {
              ...process.env,
              NO_COLOR: '1',
              HTTP_PROXY: origin,
              HTTPS_PROXY: origin,
              http_proxy: origin,
              https_proxy: origin,
              ALL_PROXY: origin,
              all_proxy: origin,
              NO_PROXY: '127.0.0.1,localhost,::1',
              no_proxy: '127.0.0.1,localhost,::1',
            },
          },
        )
        const serverProcess = backend
        backendClosed = new Promise<void>((resolve) => serverProcess.once('close', () => resolve()))
        const setupToken = await new Promise<string>((resolve, reject) => {
          let text = ''
          const safeLog = () =>
            text
              .replace(/(Stravia setup token:\s*)\S+/g, '$1[redacted]')
              .replace(/((?:setup_)?token=)[^&\s]+/g, '$1[redacted]')
          const deadline = setTimeout(() => finish(new Error(`Server readiness timed out: ${safeLog()}`)), 45_000)
          const finish = (error?: Error, token?: string) => {
            clearTimeout(deadline)
            serverProcess.stdout?.off('data', onData)
            serverProcess.stderr?.off('data', onData)
            serverProcess.off('error', onError)
            serverProcess.off('exit', onExit)
            if (error) reject(error)
            else resolve(token!)
          }
          const onData = (chunk: Buffer) => {
            text += stripVTControlCharacters(chunk.toString())
            const token = /Stravia setup token:\s*(\S+)/.exec(text)?.[1]
            const port = /Stravia Server listening[^\n]*127\.0\.0\.1:(\d+)/.exec(text)?.[1]
            if (token && port) {
              backendPort = Number(port)
              finish(undefined, token)
            }
          }
          const onError = (error: Error) => finish(error)
          const onExit = (code: number | null) =>
            finish(new Error(`Server exited before readiness (${code}): ${safeLog()}`))
          serverProcess.stdout?.on('data', onData)
          serverProcess.stderr?.on('data', onData)
          serverProcess.once('error', onError)
          serverProcess.once('exit', onExit)
        })
        await use({ origin, setupToken })
      } catch (error) {
        failed = true
        failure = error
      } finally {
        try {
          if (backend?.pid && backend.exitCode === null && backend.signalCode === null && !backend.kill()) {
            cleanupErrors.push(new Error(`Failed to stop server ${backend.pid}`))
          } else {
            await backendClosed
          }
        } catch (error) {
          cleanupErrors.push(error)
        }
        try {
          await stopProxy(proxy)
        } catch (error) {
          cleanupErrors.push(error)
        }
        try {
          await rm(directory, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 })
        } catch (error) {
          cleanupErrors.push(error)
        }
      }
      if (failed && cleanupErrors.length)
        throw new AggregateError([failure, ...cleanupErrors], 'RPM scenario and cleanup failed', { cause: failure })
      if (failed) throw failure
      if (cleanupErrors.length) throw new AggregateError(cleanupErrors, 'RPM scenario cleanup failed')
    },
    { timeout: 120_000 },
  ],
})

test('real management persists root and destination RPM limits and queue settings in both locales', async ({
  page,
  context,
  management,
}) => {
  test.setTimeout(120_000)
  await context.addInitScript(() => {
    if (!localStorage.getItem('stravia-locale')) localStorage.setItem('stravia-locale', 'en-US')
  })
  // Only the unrelated external update check is stubbed; all feature APIs are real.
  await context.route('**/api/v1/updates/check', (route) =>
    route.fulfill({
      json: {
        data: {
          current_version: '0.0.0',
          check_status: 'up-to-date',
          last_success_at: null,
          last_failure: null,
          available_update: null,
          skipped: false,
          download_supported: false,
        },
      },
    }),
  )
  const goto = (path: string) => page.goto(`${management.origin}${path}`)
  await goto('/setup')
  await expect(page.locator('#setup-token')).toBeVisible()
  await api(page, '/setup/claim', 'POST', { token: management.setupToken })
  await api(page, '/setup/complete', 'POST', {
    database: { backend: 'sqlite' },
    username: 'admin',
    password,
    client_base_url: management.origin,
  })
  await goto('/login')
  await page.locator('#admin-username').fill('admin')
  await page.locator('input[autocomplete="current-password"]').fill(password)
  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(`${management.origin}/`)

  const provider = await api<Provider>(page, '/providers', 'POST', {
    name: 'Local RPM service',
    source: {
      type: 'custom',
      vendor: 'openai-compatible',
      channel: 'default',
      protocol: 'openai-compatible',
      base_url: `${management.origin}/local-upstream/v1`,
    },
    credential: { type: 'api_key', value: 'local-test-secret' },
    use_proxy: false,
  })
  await api(page, `/providers/${provider.id}/models`, 'POST', {
    model_id: 'rpm-upstream',
    metadata: {
      id: 'rpm-upstream',
      name: 'RPM upstream',
      description: 'Preserve this description',
      limit: { context: 200000 },
      modalities: { input: ['text', 'image', 'binary'], output: ['text'] },
      reasoning_efforts: ['low', 'high', 'future'],
      cost: { input: 0.25, output: 1, cache_read: 0.025 },
    },
    template_id: null,
  })
  await api<Route>(page, '/models', 'POST', {
    model_id: 'rpm-browser-model',
    targets: [
      {
        provider_id: provider.id,
        model: 'rpm-upstream',
        enabled: true,
        priority: 1,
        thinking_level_map: ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'].map((level) => ({
          level,
          control: { type: 'hidden' },
          source: 'overridden',
        })),
      },
    ],
  })
  const key = await api<ApiKey>(page, '/api-keys', 'POST', {
    name: 'RPM browser key',
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_web_search: false,
    inject_media_understanding: false,
    inject_media_generation: false,
    model_ids: [],
  })
  const editor = page.locator('[data-slot="sheet-content"]')
  const openKey = async () => {
    await goto('/api-keys')
    await page.getByRole('row').filter({ hasText: key.name }).getByRole('cell').nth(1).click()
    await expect(page.locator('#api-key-rpm-limit')).toBeVisible()
  }
  await openKey()
  const keyLimit = page.locator('#api-key-rpm-limit')
  await expect(keyLimit).toHaveAttribute('placeholder', 'Unlimited')
  await expect(editor.getByLabel(/RPM/)).toBeVisible()
  await keyLimit.fill('7')
  await editor.getByRole('button', { name: 'Save API Key', exact: true }).click()
  await expect(editor).toBeHidden()
  expect((await api<ApiKey[]>(page, '/api-keys')).find((item) => item.id === key.id)?.rpm_limit).toBe(7)
  await openKey()
  await expect(keyLimit).toHaveValue('7')
  await keyLimit.fill('')
  await editor.getByRole('button', { name: 'Save API Key', exact: true }).click()
  await expect(editor).toBeHidden()
  expect((await api<ApiKey[]>(page, '/api-keys')).find((item) => item.id === key.id)?.rpm_limit).toBeNull()
  await openKey()
  await expect(keyLimit).toHaveValue('')
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await editor.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  await page.setViewportSize({ width: 1280, height: 800 })

  const readRpm = async () => JSON.parse(await api<string>(page, '/settings/rpm_admission')) as RpmConfig
  await goto('/settings')
  const queue = page.locator('section[aria-labelledby="rpm-wait-title"]')
  await queue.locator('#rpm-preferred-wait').fill('1500')
  await queue.locator('#rpm-total-wait').fill('9000')
  await queue.locator('#rpm-queue-capacity').fill('16')
  const saveQueue = queue.locator('button[type="submit"]')
  await saveQueue.click()
  await expect(saveQueue).toBeDisabled()
  expect(await readRpm()).toMatchObject({ preferred_wait_ms: 1500, total_wait_ms: 9000, queue_capacity: 16 })
  await page.reload()
  await expect(queue.locator('#rpm-preferred-wait')).toHaveValue('1500')
  await expect(queue.locator('#rpm-total-wait')).toHaveValue('9000')
  await expect(queue.locator('#rpm-queue-capacity')).toHaveValue('16')
  await page.screenshot({ path: test.info().outputPath('rpm-queue-en.png'), fullPage: true })

  const catalogUrl = `/providers/${encodeURIComponent(provider.id)}?view=models`
  const rpmTrigger = page.locator('.route-desktop-table').getByRole('button', { name: /^RPM limit for rpm-upstream:/ })
  const rpmDialog = page.getByRole('dialog')
  const readDestination = async () =>
    (await readRpm()).destinations.find((item) => item.provider_id === provider.id && item.model === 'rpm-upstream')
  const saveDestination = async () => {
    await rpmDialog.getByRole('button', { name: 'Save limit', exact: true }).click()
    await expect(rpmDialog).toBeHidden()
  }
  await goto(catalogUrl)
  await expect(rpmTrigger).toHaveAccessibleName('RPM limit for rpm-upstream: Unlimited')
  await rpmTrigger.click()
  await rpmDialog.getByLabel('Requests per minute', { exact: true }).fill('30')
  await saveDestination()
  expect(await readDestination()).toEqual({ provider_id: provider.id, model: 'rpm-upstream', rpm_limit: 30 })
  await expect(rpmTrigger).toHaveAccessibleName('RPM limit for rpm-upstream: 30 RPM')
  await page.reload()
  await expect(rpmTrigger).toHaveAccessibleName('RPM limit for rpm-upstream: 30 RPM')
  await goto('/settings')
  await queue.locator('#rpm-total-wait').fill('10000')
  await saveQueue.click()
  await expect(saveQueue).toBeDisabled()
  expect(await readDestination()).toEqual({ provider_id: provider.id, model: 'rpm-upstream', rpm_limit: 30 })
  await goto(catalogUrl)
  await rpmTrigger.click()
  await expect(rpmDialog.getByLabel('Requests per minute', { exact: true })).toHaveValue('30')
  await rpmDialog.getByLabel('Requests per minute', { exact: true }).fill('')
  await saveDestination()
  expect(await readDestination()).toBeUndefined()
  await page.reload()
  await expect(rpmTrigger).toHaveAccessibleName('RPM limit for rpm-upstream: Unlimited')
  expect(await readRpm()).toMatchObject({ preferred_wait_ms: 1500, total_wait_ms: 10000, queue_capacity: 16 })

  const readModel = () => api<ProviderModelDetail>(page, `/providers/${provider.id}/model?model=rpm-upstream`)
  const initialMetadata = (await readModel()).metadata
  const table = page.locator('.route-desktop-table')
  const contextTrigger = table.getByRole('button', { name: 'Context tokens rpm-upstream', exact: true })
  const cellEditor = page.locator('[data-slot="popover-content"]')
  await contextTrigger.click()
  await cellEditor.getByRole('spinbutton', { name: 'Context tokens', exact: true }).fill('256000')
  const concurrent = await readModel()
  await api(page, `/providers/${provider.id}/model`, 'PUT', {
    model_id: 'rpm-upstream',
    metadata: { ...concurrent.metadata, description: 'Changed by another administrator' },
    revision: concurrent.revision,
  })
  await cellEditor.getByRole('button', { name: 'Save model', exact: true }).click()
  await expect(cellEditor.getByRole('alert')).toBeVisible()
  await expect(cellEditor.getByRole('spinbutton', { name: 'Context tokens', exact: true })).toHaveValue('256000')
  expect((await readModel()).metadata.limit?.context).toBe(200000)
  expect((await readModel()).metadata.description).toBe('Changed by another administrator')
  await cellEditor.getByRole('button', { name: 'Cancel', exact: true }).click()
  await contextTrigger.click()
  await cellEditor.getByRole('spinbutton', { name: 'Context tokens', exact: true }).fill('256000')
  await cellEditor.getByRole('button', { name: 'Save model', exact: true }).click()
  await expect(cellEditor).toBeHidden()
  expect((await readModel()).metadata).toEqual({
    ...initialMetadata,
    description: 'Changed by another administrator',
    limit: { context: 256000 },
  })
  await expect(page).toHaveURL(`${management.origin}${catalogUrl}`)
  await table.getByRole('button', { name: 'Modalities rpm-upstream', exact: true }).click()
  await cellEditor.getByRole('button', { name: 'Generated output types', exact: true }).click()
  await page.getByRole('option', { name: 'Image', exact: true }).click()
  await page.keyboard.press('Escape')
  await cellEditor.getByRole('button', { name: 'Save model', exact: true }).click()
  await expect(cellEditor).toBeHidden()
  expect((await readModel()).metadata.modalities).toEqual({
    input: ['text', 'image', 'binary'],
    output: ['text', 'image'],
  })
  await table.getByRole('button', { name: 'Reasoning effort rpm-upstream', exact: true }).click()
  await cellEditor.getByRole('textbox', { name: 'Custom reasoning effort', exact: true }).fill('custom-effort')
  await cellEditor.getByRole('button', { name: 'Add', exact: true }).click()
  await cellEditor.getByRole('button', { name: 'Save model', exact: true }).click()
  await expect(cellEditor).toBeHidden()
  expect((await readModel()).metadata.reasoning_efforts).toEqual(['low', 'high', 'future', 'custom-effort'])
  expect((await readModel()).metadata.cost).toEqual(initialMetadata.cost)
  await table.getByRole('button', { name: 'Availability when adding models rpm-upstream', exact: true }).click()
  await cellEditor.getByRole('button', { name: 'Availability when adding models', exact: true }).click()
  await page.getByRole('option', { name: 'Always allow', exact: true }).click()
  await expect(cellEditor).toBeHidden()
  expect((await readModel()).selection_policy).toBe('force_enabled')

  await goto(`${catalogUrl}&model=rpm-upstream`)
  const detailRpm = page.locator('section[aria-labelledby="provider-model-rpm-title"]')
  await detailRpm.getByRole('spinbutton', { name: 'Requests per minute', exact: true }).fill('21')
  await detailRpm.getByRole('button', { name: 'Save limit', exact: true }).click()
  await expect.poll(async () => (await readDestination())?.rpm_limit).toBe(21)
  expect(await readRpm()).toMatchObject({ preferred_wait_ms: 1500, total_wait_ms: 10000, queue_capacity: 16 })
  await page.reload()
  await expect(detailRpm.getByRole('spinbutton', { name: 'Requests per minute', exact: true })).toHaveValue('21')
  await detailRpm.getByRole('spinbutton', { name: 'Requests per minute', exact: true }).fill('')
  await detailRpm.getByRole('button', { name: 'Save limit', exact: true }).click()
  await expect.poll(readDestination).toBeUndefined()

  // Switch through the same persisted local preference used by the app.
  await page.evaluate(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  await page.setViewportSize({ width: 390, height: 844 })
  await goto('/api-keys')
  await page.locator('.route-mobile-list').getByText(key.name, { exact: true }).click()
  await expect(keyLimit).toBeVisible()
  await expect(keyLimit).toHaveValue('')
  await expect(keyLimit).toHaveAttribute('placeholder', '不限')
  await expect(editor.getByLabel(/RPM/)).toBeVisible()
  expect(await editor.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  await editor.getByRole('button', { name: '取消', exact: true }).click()
  await goto(catalogUrl)
  const mobileRpmTrigger = page
    .locator('.route-mobile-list')
    .getByRole('button', { name: /^rpm-upstream 的 RPM 限额：/ })
  await expect(mobileRpmTrigger).toHaveAccessibleName('rpm-upstream 的 RPM 限额：不限')
  await mobileRpmTrigger.click()
  const mobileLimit = rpmDialog.getByLabel('每分钟请求数', { exact: true })
  await expect(mobileLimit).toHaveAttribute('placeholder', '不限')
  await mobileLimit.fill('18')
  await rpmDialog.getByRole('button', { name: '保存限额', exact: true }).click()
  await expect(rpmDialog).toBeHidden()
  await page.reload()
  await expect(mobileRpmTrigger).toHaveAccessibleName('rpm-upstream 的 RPM 限额：18 RPM')
  await mobileRpmTrigger.click()
  await expect(mobileLimit).toHaveValue('18')
  await mobileLimit.fill('')
  await rpmDialog.getByRole('button', { name: '保存限额', exact: true }).click()
  await expect(rpmDialog).toBeHidden()
  expect(await readDestination()).toBeUndefined()
  await goto('/settings')
  await expect(queue.locator('#rpm-queue-capacity')).toHaveValue('16')
  await queue.locator('#rpm-queue-capacity').fill('24')
  await saveQueue.click()
  await expect(saveQueue).toBeDisabled()
  await page.reload()
  await expect(queue.locator('#rpm-queue-capacity')).toHaveValue('24')
  expect((await readRpm()).queue_capacity).toBe(24)
  expect(await queue.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: test.info().outputPath('rpm-queue-zh-CN.png'), fullPage: true })
})
