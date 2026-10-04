import { test, expect } from '@playwright/test'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { stripVTControlCharacters } from 'node:util'
import { createServer } from 'node:http'
import { once } from 'node:events'

// Uses the existing production-server setup/session APIs; no observation route is intercepted.
test('real Chromium selects, switches and closes native HTTP observation scopes', async ({ browser }) => {
  test.setTimeout(120_000)
  const directory = await mkdtemp(join(tmpdir(), 'stravia-observation-browser-'))
  const binary =
    process.env.STRAVIA_BINARY ??
    resolve(
      import.meta.dirname,
      '../../../target/release',
      process.platform === 'win32' ? 'stravia-server.exe' : 'stravia-server',
    )
  const server = spawn(binary, ['--host', '127.0.0.1', '--port', '0', '--data-dir', directory], {
    cwd: directory,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, NO_COLOR: '1' },
  })
  const closed = new Promise<void>((resolve) => server.once('close', () => resolve()))
  const context = await browser.newContext()
  const provider = createServer((request, response) => {
    if (request.method === 'GET' && request.url?.endsWith('/models')) {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(
        JSON.stringify({
          object: 'list',
          data: [{ id: 'synthetic', object: 'model', created: 1, owned_by: 'synthetic' }],
        }),
      )
      return
    }
    let body = ''
    request.on('data', (chunk) => {
      body += chunk.toString()
    })
    request.on('end', () => {
      const payload = JSON.parse(body) as { stream?: boolean; messages?: Array<{ content: string }> }
      const text = payload.messages?.at(-1)?.content ?? 'synthetic answer'
      if (payload.stream) {
        response.writeHead(200, { 'content-type': 'text/event-stream' })
        const chunk = (delta: object, finish_reason: string | null, usage?: object) => ({
          id: 'synthetic-completion',
          object: 'chat.completion.chunk',
          created: 1,
          model: 'synthetic',
          choices: [{ index: 0, delta, finish_reason }],
          ...(usage ? { usage } : {}),
        })
        response.end(
          `data: ${JSON.stringify(chunk({ role: 'assistant', content: text }, null))}\n\ndata: ${JSON.stringify(chunk({}, 'stop', { prompt_tokens: 600, completion_tokens: 600, total_tokens: 1200 }))}\n\ndata: [DONE]\n\n`,
        )
        return
      }
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(
        JSON.stringify({
          id: 'synthetic-completion',
          object: 'chat.completion',
          created: 1,
          model: 'synthetic',
          choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 600, completion_tokens: 600, total_tokens: 1200 },
        }),
      )
    })
  })
  provider.listen(0, '127.0.0.1')
  await once(provider, 'listening')
  try {
    const ready = await new Promise<{ origin: string; token: string }>((resolve, reject) => {
      let logs = ''
      const timeout = setTimeout(() => reject(new Error('Isolated production server did not become ready')), 45_000)
      const receive = (chunk: Buffer) => {
        logs += stripVTControlCharacters(chunk.toString())
        const token = /Stravia setup token:\s*(\S+)/.exec(logs)?.[1]
        const port = /(?:Stravia Server listening|startup listener opened)[^\n]*127\.0\.0\.1:(\d+)/.exec(logs)?.[1]
        if (token && port) {
          clearTimeout(timeout)
          resolve({ origin: `http://127.0.0.1:${port}`, token })
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
    await context.addInitScript(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    const page = await context.newPage()
    await page.goto(`${ready.origin}/setup`)
    const api = async (path: string, method = 'GET', body?: unknown) => {
      // Setup can navigate the document while completing; the shared cookie
      // context keeps fixture preparation independent of that document lifetime.
      const response = await context.request.fetch(`${ready.origin}/api/v1${path}`, {
        method,
        headers: { 'content-type': 'application/json', 'x-stravia-csrf': '1', origin: ready.origin },
        data: body,
      })
      if (!response.ok()) throw new Error(`${method} ${path}: ${response.status()}`)
      return response.status() === 204 ? undefined : (await response.json()).data
    }
    await api('/setup/claim', 'POST', { token: ready.token })
    await api('/setup/complete', 'POST', {
      database: { backend: 'sqlite' },
      username: 'admin',
      password: 'isolated observation password',
      client_base_url: ready.origin,
    })
    await page.goto(`${ready.origin}/login`)
    await page.locator('#admin-username').fill('admin')
    await page.locator('input[autocomplete="current-password"]').fill('isolated observation password')
    await page.locator('button[type="submit"]').click()
    await expect(page).toHaveURL(`${ready.origin}/`)
    const address = provider.address()
    if (!address || typeof address === 'string') throw new Error('Local provider has no TCP address')
    const service = await api('/providers', 'POST', {
      name: 'isolated synthetic provider',
      source: {
        type: 'custom',
        vendor: 'custom',
        channel: 'default',
        protocol: 'openai-compatible',
        base_url: `http://127.0.0.1:${address.port}`,
      },
      credential: { type: 'api_key', value: 'synthetic-only' },
      vendor_options: {},
    })
    await api(`/providers/${service.id}/models`, 'POST', {
      model_id: 'synthetic',
      metadata: { name: 'Browser synthetic model' },
    })
    const model = await api('/models', 'POST', {
      model_id: 'browser-synthetic',
      display_name: 'Browser synthetic',
      targets: [{ provider_id: service.id, model: 'synthetic' }],
    })
    const clientKey = await api('/api-keys', 'POST', { name: 'isolated inference key', model_ids: [model.id] })
    const expected = ['Selected A 中文 e\u0301 👩🏽‍💻', 'Selected B safe **Markdown**']
    const interactions: Array<{ id: string; text: string }> = []
    for (const text of expected) {
      const result = await context.request.post(`${ready.origin}/v1/chat/completions`, {
        headers: { Authorization: `Bearer ${clientKey.key}` },
        data: { model: 'browser-synthetic', messages: [{ role: 'user', content: text }] },
      })
      expect(result.ok()).toBe(true)
      let id: string | undefined
      await expect
        .poll(async () => {
          const forest = await api(
            `/observations/interactions?start_at=${Date.now() - 60_000}&end_at=${Date.now() + 60_000}&limit=30&model=${model.id}`,
          )
          const members = forest.roots.flatMap(
            (root: { interactions: Array<{ id: string; status: string }> }) => root.interactions,
          )
          id = members.find(
            (item: { id: string; status: string }) =>
              item.status === 'completed' && !interactions.some((previous) => previous.id === item.id),
          )?.id
          return id
        })
        .toBeDefined()
      interactions.push({ id: id!, text })
    }
    const scopeRequests: string[] = []
    page.on('request', (request) => {
      if (/\/interactions\/[^/]+\/live(?:\?|$)/.test(request.url())) scopeRequests.push(request.url())
    })
    for (const [index, text] of expected.entries()) {
      await page.evaluate((locale) => localStorage.setItem('stravia-locale', locale), index === 0 ? 'en-US' : 'zh-CN')
      await page.emulateMedia({ reducedMotion: index === 0 ? 'no-preference' : 'reduce' })
      await page.setViewportSize(index === 0 ? { width: 1280, height: 800 } : { width: 320, height: 740 })
      const selected = interactions[index]
      await page.goto(`${ready.origin}/logs?interaction=${selected.id}`)
      const details = page
        .locator('[aria-label="Observation details"]:visible, [role="dialog"]:visible')
        .filter({ has: page.getByRole('log') })
      await expect(details.getByRole('log')).toContainText(text.replaceAll('**', ''))
      await expect.poll(() => scopeRequests.some((url) => url.includes(`/interactions/${selected.id}/live`))).toBe(true)
      await page.keyboard.press('Escape')
      await expect(details).toHaveCount(0)
    }
    expect(
      scopeRequests.every((url) =>
        interactions.some((item: { id: string }) => url.includes(`/interactions/${item.id}/live`)),
      ),
    ).toBe(true)
    await page.evaluate(() => localStorage.setItem('stravia-locale', 'en-US'))
    await page.setViewportSize({ width: 1280, height: 800 })
    // Two genuine rejected client requests create separate persisted diagnostic records.
    // No provider is needed: unknown routes are real zero-output failures, not injected observations.
    const key = await api('/api-keys', 'POST', { name: 'isolated browser observation key', model_ids: [] })
    for (const model of ['browser-rejected-a', 'browser-rejected-b']) {
      const response = await context.request.post(`${ready.origin}/v1/responses`, {
        headers: { Authorization: `Bearer ${key.key}` },
        data: { model, input: 'isolated synthetic request' },
      })
      expect(response.ok()).toBe(false)
    }
    await expect
      .poll(async () => {
        const failed = await api(
          `/observations/failed-requests?start_at=${Date.now() - 60_000}&end_at=${Date.now() + 60_000}&limit=30`,
        )
        return failed.items.map((item: { model: string | null }) => item.model)
      })
      .toEqual(expect.arrayContaining(['browser-rejected-a', 'browser-rejected-b']))
    const observationRequests: string[] = []
    page.on('request', (request) => {
      if (request.url().includes('/observations/')) observationRequests.push(request.url())
    })
    await page.goto(`${ready.origin}/logs`)
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const table = page.getByRole('table')
    await expect(table).toContainText('browser-rejected-a')
    await expect(table).toContainText('browser-rejected-b')
    const rows = table.getByRole('row').filter({ has: page.getByRole('button') })
    await rows.filter({ hasText: 'browser-rejected-a' }).getByRole('button').first().click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector).toContainText('browser-rejected-a')
    await inspector.getByRole('button', { name: 'Close', exact: true }).click()
    await rows.filter({ hasText: 'browser-rejected-b' }).getByRole('button').first().click()
    await expect(inspector).toContainText('browser-rejected-b')
    await expect(inspector).not.toContainText('browser-rejected-a')
    await inspector.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(inspector).toHaveCount(0)
    expect(observationRequests.some((url) => url.includes('/summary'))).toBe(false)
    expect(observationRequests.some((url) => /\/interactions\/[^/]+\/live/.test(url))).toBe(false)
  } finally {
    await context.close()
    provider.closeAllConnections()
    if (provider.listening)
      await new Promise<void>((resolve, reject) => provider.close((error) => (error ? reject(error) : resolve())))
    if (server.exitCode === null) server.kill()
    await closed
    await rm(directory, { recursive: true, force: true })
  }
})
