import { createServer } from 'node:net'
import { createServer as createProvider } from 'node:http'
import { execFile, execFileSync } from 'node:child_process'
import type { AddressInfo } from 'node:net'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { basename, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'

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

async function expectPortSwitch(previousTimeOrigin: number, port: number): Promise<void> {
  // 端口切换会重载 WebView；旧文档中的异步 IPC 回调会随文档销毁，不能用它轮询切换。
  await browser.waitUntil(
    () =>
      browser.execute(
        (previous) => performance.timeOrigin !== previous && document.readyState === 'complete',
        previousTimeOrigin,
      ),
    { timeout: 10_000, timeoutMsg: 'desktop page did not reload after switching the listener' },
  )
  const state = (await browser.tauri.execute(({ core }) => core.invoke('get_desktop_port_state'))) as DesktopPortState
  expect(state.mode).toBe('fixed')
  expect(state.currentPort).toBe(port)
  expect(state.fixedPort).toBe(port)
  await expect(browser).toHaveUrl(expect.stringContaining('/settings'))
  await expect($('#desktop-fixed-port')).toHaveValue(String(port))
  await expect($('header button[aria-expanded]')).toBeDisplayed()
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
  before(async () => {
    await browser.tauri.switchWindow('main')
    await $('a[href="/vendor-plugins"]').waitForExist({ timeout: 180_000 })
  })

  it('switches and closes real selected scopes and zero-output diagnostics through native authenticated HTTP', async () => {
    await browser.execute(() => localStorage.setItem('stravia-locale', 'en-US'))
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    const port = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const suffix = Date.now().toString(36)
    const models = [`native-rejected-a-${suffix}`, `native-rejected-b-${suffix}`]
    const key = (await adminRequest(port, '/api-keys', {
      method: 'POST',
      body: JSON.stringify({ name: `Native observation ${suffix}`, model_ids: [] }),
    })) as { id: string; key: string }
    const upstream = createProvider((request, response) => {
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
      let text = ''
      request.on('data', (chunk) => {
        text += chunk.toString()
      })
      request.on('end', () => {
        const payload = JSON.parse(text) as { stream?: boolean; messages: Array<{ content: string }> }
        if (payload.stream) {
          response.writeHead(200, { 'content-type': 'text/event-stream' })
          const chunk = (delta: object, finish_reason: string | null, usage?: object) => ({
            id: 'native-synthetic',
            object: 'chat.completion.chunk',
            created: 1,
            model: 'synthetic',
            choices: [{ index: 0, delta, finish_reason }],
            ...(usage ? { usage } : {}),
          })
          response.end(
            `data: ${JSON.stringify(chunk({ role: 'assistant', content: payload.messages.at(-1)?.content }, null))}\n\ndata: ${JSON.stringify(chunk({}, 'stop', { prompt_tokens: 600, completion_tokens: 12000, total_tokens: 12600 }))}\n\ndata: [DONE]\n\n`,
          )
          return
        }
        response.writeHead(200, { 'content-type': 'application/json' })
        response.end(
          JSON.stringify({
            id: 'native-synthetic',
            object: 'chat.completion',
            created: 1,
            model: 'synthetic',
            choices: [
              {
                index: 0,
                message: { role: 'assistant', content: payload.messages.at(-1)?.content },
                finish_reason: 'stop',
              },
            ],
            usage: { prompt_tokens: 600, completion_tokens: 12000, total_tokens: 12600 },
          }),
        )
      })
    })
    await new Promise<void>((resolve) => upstream.listen(0, '127.0.0.1', resolve))
    const upstreamPort = (upstream.address() as AddressInfo).port
    let serviceId: string | undefined
    let modelId: string | undefined
    try {
      const service = createdResource(
        await adminRequest(port, '/providers', {
          method: 'POST',
          body: JSON.stringify({
            name: `Native observation provider ${suffix}`,
            source: {
              type: 'custom',
              vendor: 'custom',
              channel: 'default',
              protocol: 'openai-compatible',
              base_url: `http://127.0.0.1:${upstreamPort}`,
            },
            credential: { type: 'api_key', value: 'synthetic-only' },
            vendor_options: {},
          }),
        }),
        'Provider',
      )
      serviceId = service.id
      await adminRequest(port, `/providers/${service.id}/models`, {
        method: 'POST',
        body: JSON.stringify({ model_id: 'synthetic', metadata: { name: 'Native observation model' } }),
      })
      const route = createdResource(
        await adminRequest(port, '/models', {
          method: 'POST',
          body: JSON.stringify({
            model_id: `native-observation-${suffix}`,
            display_name: 'Native observation',
            targets: [{ provider_id: service.id, model: 'synthetic' }],
          }),
        }),
        'Model',
      )
      modelId = route.id
      const members: Array<{ id: string; content: string }> = []
      for (const content of ['Native selected A', 'Native selected B']) {
        const response = await fetch(`http://127.0.0.1:${port}/v1/chat/completions`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', Authorization: `Bearer ${key.key}` },
          body: JSON.stringify({ model: `native-observation-${suffix}`, messages: [{ role: 'user', content }] }),
        })
        expect(response.ok).toBe(true)
        let id: string | undefined
        await browser.waitUntil(
          async () => {
            const forest = (await adminRequest(
              port,
              `/observations/interactions?start_at=${Date.now() - 60_000}&end_at=${Date.now() + 60_000}&limit=30&model=${route.id}`,
            )) as { roots: Array<{ interactions: Array<{ id: string; status: string }> }> }
            id = forest.roots
              .flatMap((root) => root.interactions)
              .find((item) => item.status === 'completed' && !members.some((previous) => previous.id === item.id))?.id
            return Boolean(id)
          },
          { timeoutMsg: 'Native inference did not persist its completed observation' },
        )
        members.push({ id: id!, content })
      }
      // 页面外写入夹具后重新加载，避免复用启动时的空统计缓存。
      await browser.refresh()
      await browser.tauri.switchWindow('main')
      await browser.maximizeWindow()
      // 真实原生宿主使用刚完成的本地调用，不以浏览器桩响应代替 IPC 统计与绘图。
      {
        await $('a[href="/stats"]').click()
        const section = await $('[aria-labelledby="latency-trend-title"]')
        await section.waitForExist()
        const chart = await section.$('[aria-label="Latency and speed chart"]')
        await chart.waitForExist()
        await chart.scrollIntoView({ block: 'center' })
        await expect(chart).toBeDisplayed()
        await expect(chart.$('[aria-label="Time to first token axis"]')).toBeDisplayed()
        await expect(chart.$('[aria-label="TPS axis"]')).toBeDisplayed()
        const styles = await browser.execute(() => {
          const chart = document.querySelector('[aria-label="Latency and speed chart"]')!
          const first = chart.querySelector('[aria-label="Time to first token"]')!
          const tps = chart.querySelector('[aria-label="TPS"]')!
          return { first: getComputedStyle(first).strokeDasharray, tps: getComputedStyle(tps).strokeDasharray }
        })
        expect(styles.first).toBe('none')
        expect(styles.tps).not.toBe('none')
        // 嵌入式驱动的指针移动依赖前台焦点；在真实 WebView 发送事件，不扩展窗口权限。
        await browser.execute(() => {
          const svg = document.querySelector('[aria-label="Latency and speed chart"] svg')!
          const bounds = svg.getBoundingClientRect()
          svg.dispatchEvent(
            new PointerEvent('pointermove', {
              bubbles: true,
              pointerType: 'mouse',
              clientX: bounds.x + bounds.width / 2,
              clientY: bounds.y + bounds.height / 2,
            }),
          )
        })
        await expect($('[role="tooltip"]')).toBeDisplayed()
        await expect($('[role="tooltip"]')).toHaveText(expect.stringContaining('tok/s'))
        await expect($('[role="tooltip"]')).toHaveText(expect.stringMatching(/\d(?:\.\d+)? s/))
      }
      await $('a[href="/logs"]').click()
      await $('button[aria-label="Load and show all chains"]').waitForEnabled()
      await browser.execute(() => {
        const scopes: Array<{ id: string; status?: number; sse: boolean; aborted: boolean }> = []
        const original = window.fetch.bind(window)
        Object.assign(window, { nativeObservationScopes: scopes })
        window.fetch = (input, init) => {
          const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
          const id = /\/observations\/interactions\/([^/]+)\/live(?:\?|$)/.exec(url)?.[1]
          const result = original(input, init)
          if (!id) return result
          const scope = { id, sse: false, aborted: false, status: undefined as number | undefined }
          scopes.push(scope)
          const signal = init?.signal ?? (input instanceof Request ? input.signal : undefined)
          signal?.addEventListener('abort', () => (scope.aborted = true), { once: true })
          return result.then((response) => {
            scope.status = response.status
            scope.sse = response.headers.get('content-type')?.includes('text/event-stream') ?? false
            return response
          })
        }
      })
      let previous: { id: string; content: string } | undefined
      for (const content of ['Native selected A', 'Native selected B']) {
        const selected = members.find((member) => member.content === content)
        expect(selected).toBeDefined()
        await $('button[aria-label="Load and show all chains"]').click()
        await $(`.svelte-flow__node[data-id="${selected!.id}"] h3`).click()
        const reopened = await $('[aria-label="Observation details"]')
        await expect(reopened.$('[role="log"]')).toHaveText(expect.stringContaining(content))
        await browser.waitUntil(
          () =>
            browser.execute((id) => {
              const scopes = (
                window as unknown as {
                  nativeObservationScopes: Array<{ id: string; status?: number; sse: boolean; aborted: boolean }>
                }
              ).nativeObservationScopes
              return scopes.some((scope) => scope.id === id && scope.status === 200 && scope.sse && !scope.aborted)
            }, selected!.id),
          { timeoutMsg: 'Native selected scope did not open through authenticated HTTP' },
        )
        if (previous) {
          await expect(reopened.$('[role="log"]')).not.toHaveText(expect.stringContaining(previous.content))
          await browser.waitUntil(
            () =>
              browser.execute((id) => {
                const scopes = (
                  window as unknown as { nativeObservationScopes: Array<{ id: string; aborted: boolean }> }
                ).nativeObservationScopes
                return scopes.some((scope) => scope.id === id && scope.aborted)
              }, previous!.id),
            { timeoutMsg: 'Switching the native selection did not abort the previous actual HTTP body scope' },
          )
        }
        previous = selected
      }
      const selectedDetails = await $('[aria-label="Observation details"]')
      await selectedDetails.$('button[aria-label="Close"]').click()
      await browser.waitUntil(
        () =>
          browser.execute(() => {
            const scopes = (window as unknown as { nativeObservationScopes: Array<{ aborted: boolean }> })
              .nativeObservationScopes
            return scopes.every((scope) => scope.aborted)
          }),
        { timeoutMsg: 'Closing the native inspector did not abort its actual HTTP body scope' },
      )
      await expect(selectedDetails).not.toExist()
      for (const model of models) {
        const response = await fetch(`http://127.0.0.1:${port}/v1/responses`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', Authorization: `Bearer ${key.key}` },
          body: JSON.stringify({ model, input: 'isolated native synthetic request' }),
        })
        expect(response.ok).toBe(false)
      }
      await browser.waitUntil(
        async () => {
          const failures = (await adminRequest(
            port,
            `/observations/failed-requests?start_at=${Date.now() - 60_000}&end_at=${Date.now() + 60_000}&limit=30`,
          )) as { items: Array<{ model: string | null }> }
          return models.every((model) => failures.items.some((item) => item.model === model))
        },
        { timeoutMsg: 'Native rejected requests did not persist both diagnostic records' },
      )
      await $('a[href="/logs"]').click()
      await $('button=Failed Requests').click()
      for (const model of models) {
        const row = await $(`//tr[contains(normalize-space(), "${model}")]`)
        await row.$('button').click()
        const inspector = await $('[aria-label="Observation details"]')
        await expect(inspector).toHaveText(expect.stringContaining(model))
        await inspector.$('button[aria-label="Close"]').click()
        await expect(inspector).not.toExist()
      }
    } finally {
      upstream.closeAllConnections()
      if (upstream.listening)
        await new Promise<void>((resolve, reject) => upstream.close((error) => (error ? reject(error) : resolve())))
      await adminRequest(port, `/api-keys/${key.id}`, { method: 'DELETE' })
      if (modelId) await adminRequest(port, `/models/${modelId}`, { method: 'DELETE' })
      if (serviceId) await adminRequest(port, `/providers/${serviceId}`, { method: 'DELETE' })
    }
  })

  it('shows the native browser chooser and retains an invalid path draft without saving it', async () => {
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    await $('a[href="/web-search"]').click()
    const localRow = await $(
      '//*[@id="web-search-sources"]//div[contains(@class,"grid")][.//p[normalize-space()="Local"]]',
    )
    await localRow.$('button=Edit').click()
    const chooser = await $('#web-provider-browser-choose')
    await expect(chooser).toBeDisplayed()
    const path = await $('#web-provider-browser-path')
    const savedPath = await path.getValue()
    const invalidPath = resolve(process.cwd(), 'missing-stravia-chrome-executable')
    await path.setValue(invalidPath)
    await $('[role="dialog"]').$('button=Save service').click()
    await expect($('[role="dialog"] [role="alert"]')).toBeDisplayed()
    await expect(path).toHaveValue(invalidPath)
    await $('[role="dialog"]').$('button=Cancel').click()
    await localRow.$('button=Edit').click()
    await expect($('#web-provider-browser-path')).toHaveValue(savedPath)
    await $('[role="dialog"]').$('button=Cancel').click()
  })

  it('uses the docked chat composer, effort overlay and original image input in the actual WebView', async () => {
    await browser.execute(() => localStorage.setItem('stravia-locale', 'en-US'))
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    const port = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const suffix = Date.now().toString(36)
    const upstreamInputs: unknown[] = []
    const upstream = createProvider((request, response) => {
      let body = ''
      request.on('data', (chunk) => {
        body += chunk.toString()
      })
      request.on('end', () => {
        if (request.method !== 'POST' || !request.url?.endsWith('/responses')) {
          response.writeHead(404)
          response.end()
          return
        }
        const input = JSON.parse(body) as { input: unknown }
        upstreamInputs.push(input.input)
        const output = [
          {
            type: 'message',
            id: 'native-answer',
            role: 'assistant',
            status: 'completed',
            content: [{ type: 'output_text', text: 'Native original image received', annotations: [] }],
          },
        ]
        // Exercise the same complete Responses lifecycle as the real Server
        // image proof, including the text deltas consumed by streaming clients.
        const result = {
          id: 'native-image-response',
          object: 'response',
          created_at: Math.floor(Date.now() / 1000),
          status: 'completed',
          model: 'native-visual',
          output,
          usage: {
            input_tokens: 17,
            output_tokens: 9,
            total_tokens: 26,
            input_tokens_details: { cached_tokens: 0 },
            output_tokens_details: { reasoning_tokens: 0 },
          },
          error: null,
          incomplete_details: null,
          tools: [],
          tool_choice: 'auto',
          parallel_tool_calls: true,
          store: false,
          completed_at: Math.floor(Date.now() / 1000),
          previous_response_id: null,
          instructions: null,
          truncation: 'disabled',
          text: { format: { type: 'text' }, verbosity: 'medium' },
          top_p: 1,
          presence_penalty: 0,
          frequency_penalty: 0,
          top_logprobs: 0,
          temperature: 1,
          reasoning: { effort: null, summary: 'auto' },
          max_output_tokens: null,
          max_tool_calls: null,
          background: false,
          service_tier: 'auto',
          metadata: {},
          safety_identifier: null,
          prompt_cache_key: null,
        }
        const message = output[0]
        const content = message.content[0]
        const events = [
          {
            type: 'response.created',
            response: { ...result, status: 'in_progress', completed_at: null, output: [], usage: null },
          },
          {
            type: 'response.output_item.added',
            output_index: 0,
            item: { ...message, status: 'in_progress', content: [] },
          },
          {
            type: 'response.content_part.added',
            output_index: 0,
            content_index: 0,
            item_id: message.id,
            part: { ...content, text: '' },
          },
          {
            type: 'response.output_text.delta',
            output_index: 0,
            content_index: 0,
            item_id: message.id,
            delta: content.text,
          },
          {
            type: 'response.output_text.done',
            output_index: 0,
            content_index: 0,
            item_id: message.id,
            text: content.text,
          },
          { type: 'response.content_part.done', output_index: 0, content_index: 0, item_id: message.id, part: content },
          { type: 'response.output_item.done', output_index: 0, item: message },
          { type: 'response.completed', response: result },
        ]
        response.writeHead(200, { 'content-type': 'text/event-stream' })
        response.end(
          events
            .map(
              (event, sequence_number) =>
                `event: ${event.type}\ndata: ${JSON.stringify({ ...event, sequence_number })}\n\n`,
            )
            .join(''),
        )
      })
    })
    await new Promise<void>((resolve) => upstream.listen(0, '127.0.0.1', resolve))
    let serviceId: string | undefined
    let routeId: string | undefined
    let keyId: string | undefined
    try {
      const service = createdResource(
        await adminRequest(port, '/providers', {
          method: 'POST',
          body: JSON.stringify({
            name: `Native chat service ${suffix}`,
            source: {
              type: 'custom',
              vendor: 'custom',
              channel: 'default',
              protocol: 'open-responses',
              base_url: `http://127.0.0.1:${(upstream.address() as AddressInfo).port}`,
            },
            credential: { type: 'api_key', value: 'synthetic-only' },
            vendor_options: {},
          }),
        }),
        'Provider',
      )
      serviceId = service.id
      await adminRequest(port, `/providers/${service.id}/models`, {
        method: 'POST',
        body: JSON.stringify({
          model_id: 'native-visual',
          metadata: {
            name: 'Native visual',
            modalities: { input: ['text', 'image'], output: ['text'] },
            reasoning_efforts: ['low', 'high'],
          },
        }),
      })
      const route = createdRoute(
        await adminRequest(port, '/models', {
          method: 'POST',
          body: JSON.stringify({
            model_id: `native-chat-${suffix}`,
            display_name: `Native chat ${suffix}`,
            targets: [{ provider_id: service.id, model: 'native-visual' }],
          }),
        }),
      )
      routeId = route.id
      const keyName = `Native chat Key ${suffix}`
      keyId = createdResource(
        await adminRequest(port, '/api-keys', {
          method: 'POST',
          body: JSON.stringify({ name: keyName, model_ids: [route.id] }),
        }),
        'Key',
      ).id
      await $('a[href="/"]').click()
      // SvelteKit navigation is asynchronous; refreshing before it commits can
      // reload the previous smoke's /web-search page instead of the chat page.
      await browser.waitUntil(() => browser.execute(() => location.pathname === '/' && !location.search), {
        timeoutMsg: 'Native chat navigation did not reach a new conversation',
      })
      await browser.refresh()
      await browser.tauri.switchWindow('main')
      await $('#chat-key').waitForExist()
      await $('#chat-key').click()
      // The embedded bridge's synthetic click does not emit Select's pointer
      // events; use the same keyboard interaction as the native Connect proof.
      await browser.keys('ArrowDown')
      await expect($(`//*[@role="option" and contains(normalize-space(), "${keyName}")]`)).toBeDisplayed()
      await browser.keys(keyName)
      await browser.keys('Enter')
      await expect($('#chat-key')).toHaveText(expect.stringContaining(keyName))
      await $('button[aria-label="Model and reasoning effort"]').click()
      await $('#chat-model').click()
      await browser.keys('ArrowDown')
      await expect($(`//*[@role="option" and contains(normalize-space(), "Native chat ${suffix}")]`)).toBeDisplayed()
      await browser.keys(`Native chat ${suffix}`)
      await browser.keys('Enter')
      await expect($('#chat-model')).toHaveText(expect.stringContaining(`Native chat ${suffix}`))
      await $('button*=Reasoning effort').click()
      await expect($('[role="slider"]')).toBeDisplayed()
      await browser.keys('Escape')
      const base64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a9l8AAAAASUVORK5CYII='
      await browser.execute((encoded) => {
        const clipboard = new DataTransfer()
        clipboard.items.add(
          new File([Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0))], 'native.png', {
            type: 'image/png',
          }),
        )
        document
          .querySelector('textarea')!
          .dispatchEvent(new ClipboardEvent('paste', { clipboardData: clipboard, bubbles: true }))
      }, base64)
      await expect($('img[alt="native.png"]')).toBeDisplayed()
      await expect($('button[aria-label="Send"]')).toBeDisabled()
      await $('#chat-message').setValue('Native screenshot question')
      await $('button[aria-label="Send"]').click()
      await expect($('article[aria-label="Assistant response"]')).toHaveText(
        expect.stringContaining('Native original image received'),
      )
      expect(JSON.stringify(upstreamInputs)).toContain(`data:image/png;base64,${base64}`)
      await browser.refresh()
      await expect($('article[aria-label="Your message"] img[alt="native.png"]')).toBeDisplayed()
      await expect($('button[aria-label="Add images"]')).toBeDisplayed()
      const geometry = await browser.execute(() => {
        const message = document.querySelector('#chat-message')!.getBoundingClientRect()
        const main = document.querySelector('main')!.getBoundingClientRect()
        return { top: message.top, bottom: message.bottom, mainBottom: main.bottom }
      })
      expect(geometry.bottom).toBeLessThanOrEqual(geometry.mainBottom)
      expect(geometry.top).toBeGreaterThan(geometry.mainBottom / 2)
      await mkdir(resolve(import.meta.dirname, '../../../target/test-results/chat-presentation'), { recursive: true })
      await browser.saveScreenshot(
        resolve(import.meta.dirname, '../../../target/test-results/chat-presentation/desktop.png'),
      )
    } finally {
      if (keyId) await adminRequest(port, `/api-keys/${keyId}`, { method: 'DELETE' })
      if (routeId) await adminRequest(port, `/models/${routeId}`, { method: 'DELETE' })
      if (serviceId) await adminRequest(port, `/providers/${serviceId}`, { method: 'DELETE' })
      upstream.closeAllConnections()
      await new Promise<void>((resolve, reject) => upstream.close((error) => (error ? reject(error) : resolve())))
    }
  })

  it('imports a local Wasm component through native-authenticated desktop management', async () => {
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    await $('a[href="/vendor-plugins"]').click()
    const component = fileURLToPath(new URL('../../../target/vendor-test-fixtures/lifecycle-v1.wasm', import.meta.url))
    await $('#vendor-plugin-file').waitForExist()
    const contents = await readFile(component)
    // 嵌入式驱动不支持文件路径发送；分块传递真实文件，避免超过其 JSON 请求体上限。
    for (let offset = 0; offset < contents.length; offset += 256 * 1024) {
      await browser.execute(
        (encoded, name) => {
          const input = document.querySelector<HTMLInputElement>('#vendor-plugin-file')
          if (!input) throw new Error('Plugin file input is missing')
          const previous = input.files?.item(0)
          const bytes = Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0))
          const files = new DataTransfer()
          files.items.add(new File(previous ? [previous, bytes] : [bytes], name, { type: 'application/wasm' }))
          input.files = files.files
        },
        contents.subarray(offset, offset + 256 * 1024).toString('base64'),
        basename(component),
      )
    }
    await browser.execute(() => {
      const input = document.querySelector<HTMLInputElement>('#vendor-plugin-file')
      if (!input) throw new Error('Plugin file input is missing')
      input.dispatchEvent(new Event('change', { bubbles: true }))
    })
    const preview = await $('[role="dialog"]')
    await preview.waitForExist()
    await expect(preview).toHaveText(expect.stringContaining('fixture.lifecycle'))
    await preview.$('button=Apply plugin change').click()
    await preview.waitForExist({ reverse: true })

    const card = await $('//*[contains(@class,"route-plugin-list")]/li[.//*[normalize-space()="fixture.lifecycle"]]')
    await expect(card).toHaveText(expect.stringContaining('1.0.0'))
    await expect(card).toHaveText(expect.stringContaining('Local'))
    const port = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const installed = (await adminRequest(port, '/vendor-plugins')) as Array<{
      vendor_id: string
      version: string
      source: string
    }>
    expect(installed.find((plugin) => plugin.vendor_id === 'fixture.lifecycle')).toMatchObject({
      version: '1.0.0',
      source: 'local',
    })
  })

  it('persists the complete client address through native authenticated settings without weakening access checks', async () => {
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    const port = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    const original = (await adminRequest(port, '/settings/artifact_settings')) as string
    const configuration = JSON.parse(original) as { client_base_url: string }
    expect(configuration.client_base_url).toBe(`http://127.0.0.1:${port}`)
    expect((await fetch(`http://127.0.0.1:${port}/api/v1/settings/artifact_settings`)).status).toBe(401)
    try {
      await browser.refresh()
      await browser.tauri.switchWindow('main')
      await $('a[href="/settings"]').click()
      const address = await $('#artifact-client-base-url')
      await expect(address).toHaveValue(configuration.client_base_url)
      await address.setValue('https://desktop.example:9443/client/prefix')
      await $('button=Save file settings').click()
      await expect($('button=Save file settings')).not.toBeEnabled()
      await browser.refresh()
      await expect($('#artifact-client-base-url')).toHaveValue('https://desktop.example:9443/client/prefix')
      const persisted = JSON.parse((await adminRequest(port, '/settings/artifact_settings')) as string)
      expect(persisted.client_base_url).toBe('https://desktop.example:9443/client/prefix')
    } finally {
      await adminRequest(port, '/settings/artifact_settings', {
        method: 'PUT',
        body: JSON.stringify({ value: original }),
      })
    }
  })
  it('tests credential rules through the actual desktop page while protection is disabled', async () => {
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    const port = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    await adminRequest(port, '/settings/reversible_redaction_enabled', {
      method: 'PUT',
      body: JSON.stringify({ value: 'false' }),
    })
    const before = await adminRequest(port, '/reversible-redaction/discoveries')
    await browser.refresh()
    await browser.tauri.switchWindow('main')
    await $('a[href="/reversible-redaction"]').click()
    await expect($('//h1[normalize-space()="Credential Protection"]')).toBeDisplayed()
    await expect($('#reversible-redaction-enabled')).toHaveAttribute('aria-checked', 'false')

    const matchingTab = await $('button=Matching test')
    await matchingTab.click()
    await expect(matchingTab).toHaveAttribute('aria-selected', 'true')
    const sample = 'ghp_9Er8nQ3wM0tY5bS7uL4oG6xI2kC1dZaVfJpH'
    const input = await $('#credential-test-input')
    await expect(input).toBeDisplayed()
    await input.setValue(`配置😀\n${sample}`)
    await expect(input).toHaveValue(`配置😀\n${sample}`)
    await $('button=Test matching').click()
    const result = await $('[aria-labelledby="test-results-title"] li button')
    await expect(result).toBeDisplayed()
    await expect(result).toHaveText(expect.stringContaining('Line 2, column 1'))
    await result.click()
    expect(
      await browser.execute(() => {
        const input = document.getElementById('credential-test-input') as HTMLTextAreaElement
        return input.value.slice(input.selectionStart, input.selectionEnd)
      }),
    ).toBe(sample)
    expect(await adminRequest(port, '/settings/reversible_redaction_enabled')).toBe('false')
    expect(await adminRequest(port, '/reversible-redaction/discoveries')).toEqual(before)
    await $('button=Clear input and results').click()
    await expect($('#credential-test-input')).toHaveValue('')
  })

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

    await $('a[href="/"]').click()

    const navigationTrigger = await $('header button[aria-expanded]')
    await expect(navigationTrigger).toBeDisplayed()

    if (portState.mode === 'fallback') {
      await expect($('aria/Fixed port unavailable')).toBeDisplayed()
      await $('a=Resolve in Client Settings').click()
    } else if (portState.mode === 'configError') {
      await expect($('aria/Port settings unavailable')).toBeDisplayed()
      await $('a=Open Client Settings').click()
    } else {
      await $('a[href="/settings"]').click()
    }

    await expect($('//h2[normalize-space()="Client"]')).toBeDisplayed()
    await expect($('#desktop-fixed-port')).toHaveValue(String(portState.fixedPort ?? portState.currentPort))
    await expect($('input[type="password"]')).not.toExist()
    await expect($('button=Sign out')).not.toExist()

    let activePort = portState.currentPort
    if (portState.mode !== 'fixed') {
      const nextPort = await unusedPort()
      await $('#desktop-fixed-port').setValue(String(nextPort))
      const previousTimeOrigin = await browser.execute(() => performance.timeOrigin)
      await $('button=Save Port').click()
      await expectPortSwitch(previousTimeOrigin, nextPort)
      activePort = nextPort
      await expectProtectedStatus(nextPort, nativeSession.access_token)
    }

    let replacementPort = await unusedPort()
    if (replacementPort === activePort) replacementPort = await unusedPort()
    await $('#desktop-fixed-port').setValue(String(replacementPort))
    await $('button=Save Port').click()
    const confirmation = await $('[data-slot="alert-dialog-title"]')
    await expect(confirmation).toHaveText('Change the port?')
    await expect($('[role="alertdialog"]')).toHaveText(expect.stringContaining(`127.0.0.1:${activePort}`))
    const previousTimeOrigin = await browser.execute(() => performance.timeOrigin)
    await $('button=Change Port').click()
    await expectPortSwitch(previousTimeOrigin, replacementPort)
    await expectProtectedStatus(replacementPort, nativeSession.access_token)

    await expect($('//h2[normalize-space()="Updates"]')).toBeDisplayed()
    await expect($('button=Download update')).toBeDisplayed()
    await $('button=Download update').click()
    await expect($('[role="progressbar"]')).toHaveAttribute('aria-valuenow', '50')
    await expect($('button=Pause')).not.toExist()
    await expect($('button=Cancel')).not.toExist()
    await expect($('[data-slot="dialog-title"]')).toHaveText('Install Stravia 9.9.9?')
    await expect($('[data-slot="dialog-content"]').$('button=Exit and install')).toBeEnabled()
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
    await browser.execute(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
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
            source: {
              type: 'custom',
              vendor: 'custom',
              channel: 'default',
              protocol: 'open-responses',
              base_url: 'https://desktop-smoke.invalid',
            },
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

      // 原生剪贴板拒绝未聚焦的文档；window.focus() 不能突破 Windows 前台锁，由驱动在宿主进程内激活窗口才可靠。
      await browser.maximizeWindow()
      const renderedConfiguration = await browser.execute(() => {
        const preview = document.querySelector('pre.route-code-plane')
        if (!preview?.textContent) throw new Error('Connect configuration preview was empty')
        return preview.textContent
      })
      await copyConfiguration.click()
      await browser.waitUntil(
        async () => {
          // 原生读取器内比较，避免输出剪贴板内容或测试密钥；只统一 Windows CRLF，不丢弃其他空白。
          const { stdout } = await promisify(execFile)(
            'powershell.exe',
            [
              '-NoProfile',
              '-NonInteractive',
              '-STA',
              '-Command',
              '$ErrorActionPreference = \'Stop\'; $actual = [string](Get-Clipboard -Raw); $expected = $env:STRAVIA_EXPECTED_CLIPBOARD; $actual.Replace("`r`n", "`n") -ceq $expected.Replace("`r`n", "`n")',
            ],
            { windowsHide: true, env: { ...process.env, STRAVIA_EXPECTED_CLIPBOARD: renderedConfiguration } },
          )
          return stdout.trim() === 'True'
        },
        { timeout: 10_000, timeoutMsg: 'native clipboard did not contain the rendered configuration' },
      )

      await writeConfiguration.click()
      let writtenConfig: unknown
      await browser.waitUntil(
        async () => {
          // WDIO 运行于 Node，复用 Bun TOML parser，避免把完成状态或用户配置保留绑定到序列化文案。
          writtenConfig = JSON.parse(
            execFileSync(
              'bun',
              ['-e', 'process.stdout.write(JSON.stringify(Bun.TOML.parse(await Bun.stdin.text())))'],
              { input: await readFile(configPath, 'utf8'), encoding: 'utf8' },
            ),
          )
          // 已存在的文件不代表写入完成；等增量服务记录出现后再核验原有用户配置。
          if (typeof writtenConfig !== 'object' || writtenConfig === null || !('model_providers' in writtenConfig)) {
            return false
          }
          const providers = writtenConfig.model_providers
          return typeof providers === 'object' && providers !== null && 'stravia' in providers
        },
        { timeout: 10_000, timeoutMsg: 'Codex configuration did not receive the Stravia provider' },
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
    await $('a[href="/settings"]').click()
    await expect(browser).toHaveUrl(expect.stringContaining('/settings'))
    const debugSwitch = () => $('#diagnostics-debug')
    await expect(debugSwitch()).toBeEnabled()
    await expect(debugSwitch()).toHaveAttribute('role', 'switch')
    await expect(debugSwitch()).toHaveAttribute('aria-checked', 'false')
    await debugSwitch().click()
    await expect($('[role="alertdialog"]')).toBeDisplayed()
    await (await $('[role="alertdialog"]')).$('button=Enable Debug').click()
    await expect(debugSwitch()).toHaveAttribute('aria-checked', 'true')

    const rejected = await fetch(`http://127.0.0.1:${serverPort}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'desktop-observation', messages: [{ role: 'user', content: 'local smoke' }] }),
    })
    expect(rejected.status).toBe(401)
    await $('a[href="/logs"]').click()
    await expect(browser).toHaveUrl(expect.stringContaining('/logs'))
    await $('button=Failed Requests').click()
    const request = await $('table[aria-label="Failed Requests"] tbody tr')
    await expect(request).toHaveText(expect.stringContaining('Unauthenticated'))
    await expect(request).toHaveText(expect.stringContaining('HTTP 401'))
    await request.$('button').click()
    const inspector = await $('[aria-label="Observation details"]')
    await expect(inspector).toBeDisplayed()
    await expect(inspector.$('button=Debug records')).not.toExist()
    await expect(inspector.$('button=Debug bundle')).toBeEnabled()
    await expect(inspector).toHaveText(expect.stringContaining('HTTP 401'))
    await expect(inspector.$('button=Open interaction')).not.toExist()

    await $('a[href="/settings"]').click()
    await expect(browser).toHaveUrl(expect.stringContaining('/settings'))
    await debugSwitch().click()
    await expect(debugSwitch()).toHaveAttribute('aria-checked', 'false')
    await $('a[href="/logs"]').click()
    await expect(browser).toHaveUrl(expect.stringContaining('/logs'))
    await $('button=Clear history').click()
    await (await $('[role="alertdialog"]')).$('button=Clear history').click()
    await expect($('table[aria-label="Failed Requests"]')).not.toExist()
  })

  it('persists reversible redaction changes through the native management interface', async () => {
    const serverPort = (await browser.tauri.execute(({ core }) => core.invoke('get_server_port'))) as number
    await $('a[href="/reversible-redaction"]').click()
    const toggle = () => $('#reversible-redaction-enabled')
    const expectSaved = async (value: string) => {
      await expect(browser).toHaveUrl(expect.stringContaining('/reversible-redaction'))
      await expect(toggle()).toBeDisplayed()
      await expect(toggle()).toHaveAttribute('aria-checked', value)
      await expect(toggle()).toBeEnabled()
      await expect(toggle()).toHaveAttribute('aria-busy', 'false')
      expect(await adminRequest(serverPort, '/settings/reversible_redaction_enabled')).toBe(value)
    }
    await toggle().waitForEnabled()
    const original = await toggle().getAttribute('aria-checked')
    const changed = original === 'true' ? 'false' : 'true'
    try {
      await toggle().click()
      await expectSaved(changed)
      await $('a[href="/settings"]').click()
      await expect(browser).toHaveUrl(expect.stringContaining('/settings'))
      await expect(toggle()).not.toExist()
      await $('a[href="/reversible-redaction"]').click()
      // 客户端路由完成后再刷新，避免刷新仍在离开的设置页。
      await expectSaved(changed)
      await browser.refresh()
      await expectSaved(changed)
    } finally {
      await $('a[href="/reversible-redaction"]').click()
      await toggle().waitForEnabled()
      if ((await toggle().getAttribute('aria-checked')) !== original) {
        await toggle().click()
      }
      await expectSaved(original)
    }
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
