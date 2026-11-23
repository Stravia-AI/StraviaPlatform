import { test, expect } from '@playwright/test'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { stripVTControlCharacters } from 'node:util'
import { createServer } from 'node:http'
import { once } from 'node:events'
import type { Provider } from '../src/lib/types'

test('real backend records discovery, manual policy, failure and retained deleted-service history', async ({
  context,
  page,
}) => {
  test.setTimeout(120_000)
  const directory = await mkdtemp(join(tmpdir(), 'stravia-model-history-browser-'))
  let inventory = ['first-synthetic']
  let fail = false
  const upstream = createServer((request, response) => {
    if (request.method === 'GET' && request.url?.endsWith('/models')) {
      response.writeHead(fail ? 503 : 200, { 'content-type': 'application/json' })
      response.end(
        JSON.stringify(
          fail
            ? { error: 'Synthetic discovery unavailable' }
            : {
                object: 'list',
                data: inventory.map((id) => ({ id, object: 'model', owned_by: 'synthetic', created: 1 })),
              },
        ),
      )
    } else {
      response.writeHead(404)
      response.end()
    }
  })
  upstream.listen(0, '127.0.0.1')
  await once(upstream, 'listening')
  const address = upstream.address()
  if (!address || typeof address === 'string') throw new Error('Loopback upstream has no TCP address')
  const upstreamOrigin = `http://127.0.0.1:${address.port}`
  const binary =
    process.env.STRAVIA_BINARY ??
    resolve(
      import.meta.dirname,
      '../../../target/test-artifacts/services',
      process.platform === 'win32' ? 'stravia-server.exe' : 'stravia-server',
    )
  const server = spawn(
    binary,
    [
      '--host',
      '127.0.0.1',
      '--port',
      '0',
      '--data-dir',
      directory,
      '--test-catalog-base-url',
      `${upstreamOrigin}/catalog`,
    ],
    { cwd: directory, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, NO_COLOR: '1' } },
  )
  const closed = new Promise<void>((done) => server.once('close', () => done()))
  const ready = new Promise<{ origin: string; token: string }>((done, reject) => {
    let logs = ''
    const timeout = setTimeout(() => reject(new Error('Isolated server did not become ready')), 45_000)
    const receive = (chunk: Buffer) => {
      logs += stripVTControlCharacters(chunk.toString())
      const token = /Stravia setup token:\s*(\S+)/.exec(logs)?.[1]
      const port = /(?:Stravia Server listening|startup listener opened)[^\n]*127\.0\.0\.1:(\d+)/.exec(logs)?.[1]
      if (token && port) {
        clearTimeout(timeout)
        done({ origin: `http://127.0.0.1:${port}`, token })
      }
    }
    server.stdout?.on('data', receive)
    server.stderr?.on('data', receive)
    server.once('error', (error) => {
      clearTimeout(timeout)
      reject(error)
    })
    server.once('exit', (code) => {
      clearTimeout(timeout)
      reject(new Error(`Server exited before readiness: ${code}`))
    })
  })
  try {
    const { origin, token } = await ready
    const api = async <T>(path: string, method = 'GET', body?: unknown): Promise<T> => {
      const response = await context.request.fetch(`${origin}/api/v1${path}`, {
        method,
        headers: { 'content-type': 'application/json', 'x-stravia-csrf': '1', origin },
        data: body,
      })
      if (!response.ok()) throw new Error(`${method} ${path}: ${response.status()} ${await response.text()}`)
      return response.status() === 204 ? (undefined as T) : (await response.json()).data
    }
    await api('/setup/claim', 'POST', { token })
    await api('/setup/complete', 'POST', {
      database: { backend: 'sqlite' },
      username: 'admin',
      password: 'isolated model history password',
      client_base_url: origin,
    })
    await context.addInitScript(() => {
      if (location.protocol === 'http:' || location.protocol === 'https:')
        localStorage.setItem('stravia-locale', 'en-US')
    })
    await page.goto(`${origin}/login`)
    await page.locator('#admin-username').fill('admin')
    await page.locator('input[autocomplete="current-password"]').fill('isolated model history password')
    await page.locator('button[type="submit"]').click()
    await expect(page).toHaveURL(`${origin}/`)
    const provider = await api<Provider>('/providers', 'POST', {
      name: 'Browser history service',
      source: {
        type: 'custom',
        vendor: 'custom',
        channel: 'default',
        protocol: 'openai-compatible',
        base_url: upstreamOrigin,
      },
      credential: { type: 'none' },
    })
    await page.goto(`${origin}/providers/${provider.id}?view=models&sync=created`)
    const manualSync = page.getByRole('button', { name: 'Sync models', exact: true }).first()
    await expect(page.locator('[data-sonner-toast]')).toContainText('Added 1')
    inventory = ['second-synthetic']
    await manualSync.click()
    const toast = page.locator('[data-sonner-toast]').filter({ hasText: 'Added 1 · No longer offered 1' })
    await expect(toast).toBeVisible()
    await toast.getByRole('button', { name: 'View details' }).click()
    const panel = page.getByRole('dialog', { name: 'Change history' })
    const details = panel.getByLabel('View details', { exact: true })
    await expect(details).toContainText('first-synthetic')
    await expect(details).toContainText('second-synthetic')
    await page.keyboard.press('Escape')
    const detail = await api<{ revision: number }>(`/providers/${provider.id}/model?model=second-synthetic`)
    await api(`/providers/${provider.id}/model/selection`, 'PUT', {
      model_id: 'second-synthetic',
      policy: 'force_disabled',
      revision: detail.revision,
    })
    await page.getByRole('button', { name: 'Change history', exact: true }).click()
    const policyBatch = panel.getByRole('button').filter({ hasText: 'Availability policy changed' })
    await policyBatch.focus()
    await page.keyboard.press('Enter')
    await expect(panel).toContainText("Don't allow")
    await page.keyboard.press('Escape')
    await page.getByRole('switch', { name: 'Automatic sync' }).click()
    await expect(page.getByRole('switch', { name: 'Automatic sync' })).not.toBeChecked()
    fail = true
    await manualSync.click()
    await expect(page.locator('[data-sonner-toast]').filter({ hasText: 'Model sync failed' })).toBeVisible()
    await api(`/providers/${provider.id}`, 'DELETE')
    // Browse deleted-service history from the normal, nonempty list toolbar.
    await api('/providers', 'POST', {
      name: 'Remaining browser service',
      source: {
        type: 'custom',
        vendor: 'custom',
        channel: 'default',
        protocol: 'openai-compatible',
        base_url: upstreamOrigin,
      },
      credential: { type: 'none' },
    })
    await page.goto(`${origin}/providers`)
    await page.getByRole('button', { name: 'Change history', exact: true }).click()
    await expect(page.getByRole('dialog')).toContainText('Browser history service')
    await expect(page.getByRole('dialog')).toContainText('Provider deleted')
  } finally {
    // Playwright owns the context and closes it in fixture teardown, outside the
    // test's expired timeout budget. Closing it here can replace the real error.
    try {
      upstream.closeAllConnections()
      if (upstream.listening)
        await new Promise<void>((done, reject) => upstream.close((error) => (error ? reject(error) : done())))
    } finally {
      if (server.exitCode === null) server.kill()
      try {
        await closed
      } finally {
        await rm(directory, { recursive: true, force: true })
      }
    }
  }
})
