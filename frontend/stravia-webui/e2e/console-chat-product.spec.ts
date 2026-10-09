import { expect, test } from '@playwright/test'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { stripVTControlCharacters } from 'node:util'
import { createServer } from 'node:http'
import { once } from 'node:events'

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Expected an object at the gateway boundary')
  return value as Record<string, unknown>
}
function identifier(value: unknown): string {
  const item = record(value)
  if (typeof item.id !== 'string') throw new Error('Admin creation response omitted its identifier')
  return item.id
}

// The browser, IndexedDB, admin APIs and Responses gateway are all real. Only
// the isolated local provider is synthetic; no production upstream is contacted.
test('real gateway accepts persisted reasoning replay and attributes both turns to the selected Key', async ({ browser }) => {
  test.setTimeout(120_000)
  const directory = await mkdtemp(join(tmpdir(), 'stravia-console-browser-'))
  const binary = process.env.STRAVIA_BINARY ?? resolve(import.meta.dirname, '../../../target/release', process.platform === 'win32' ? 'stravia-server.exe' : 'stravia-server')
  const upstreamRequests: Record<string, unknown>[] = []
  const upstream = createServer((request, response) => {
    if (request.method === 'GET' && request.url?.endsWith('/models')) {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ object: 'list', data: [{ id: 'synthetic-reasoner', object: 'model', created: 1, owned_by: 'synthetic' }] }))
      return
    }
    if (request.method !== 'POST' || !request.url?.endsWith('/responses')) {
      response.writeHead(404)
      response.end()
      return
    }
    let body = ''
    request.on('data', (chunk) => { body += chunk.toString() })
    request.on('end', () => {
      try {
        const payload = record(JSON.parse(body))
        upstreamRequests.push(payload)
        const turn = upstreamRequests.length
        const text = turn === 1 ? 'First real gateway answer' : 'Second real gateway answer'
        const reasoning = { type: 'reasoning', id: `reason_${turn}`, summary: [{ type: 'summary_text', text: 'Synthetic reasoning summary' }], encrypted_content: 'synthetic-opaque-reasoning' }
        const message = { type: 'message', id: `msg_${turn}`, status: 'completed', role: 'assistant', content: [{ type: 'output_text', text, annotations: [] }] }
        const result = {
          id: `resp_console_${turn}`, object: 'response', created_at: Math.floor(Date.now() / 1000), status: 'completed', model: 'synthetic-reasoner',
          output: [reasoning, message], usage: { input_tokens: 17, output_tokens: 9, total_tokens: 26, input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 3 } },
          error: null, incomplete_details: null, tools: [], tool_choice: 'auto', parallel_tool_calls: true, store: false,
          completed_at: Math.floor(Date.now() / 1000), previous_response_id: null, instructions: null,
          truncation: 'disabled', text: { format: { type: 'text' }, verbosity: 'medium' }, top_p: 1,
          presence_penalty: 0, frequency_penalty: 0, top_logprobs: 0, temperature: 1,
          reasoning: { effort: null, summary: 'auto' }, max_output_tokens: null, max_tool_calls: null,
          background: false, service_tier: 'auto', metadata: {}, safety_identifier: null, prompt_cache_key: null,
        }
        const events: Array<Record<string, unknown>> = [{ type: 'response.created', response: { ...result, status: 'in_progress', completed_at: null, output: [], usage: null } }]
        events.push({ type: 'response.output_item.added', output_index: 0, item: { ...reasoning, summary: [] } })
        events.push({ type: 'response.reasoning_summary_part.added', output_index: 0, item_id: reasoning.id, summary_index: 0, part: { type: 'summary_text', text: '' } })
        events.push({ type: 'response.reasoning_summary_text.delta', output_index: 0, item_id: reasoning.id, summary_index: 0, delta: 'Synthetic reasoning summary' })
        events.push({ type: 'response.reasoning_summary_text.done', output_index: 0, item_id: reasoning.id, summary_index: 0, text: 'Synthetic reasoning summary' })
        events.push({ type: 'response.reasoning_summary_part.done', output_index: 0, item_id: reasoning.id, summary_index: 0, part: reasoning.summary[0] })
        events.push({ type: 'response.output_item.done', output_index: 0, item: reasoning })
        events.push({ type: 'response.output_item.added', output_index: 1, item: { ...message, status: 'in_progress', content: [] } })
        events.push({ type: 'response.content_part.added', output_index: 1, content_index: 0, item_id: message.id, part: { type: 'output_text', text: '', annotations: [] } })
        events.push({ type: 'response.output_text.delta', output_index: 1, content_index: 0, item_id: message.id, delta: text })
        events.push({ type: 'response.output_text.done', output_index: 1, content_index: 0, item_id: message.id, text })
        events.push({ type: 'response.content_part.done', output_index: 1, content_index: 0, item_id: message.id, part: message.content[0] })
        events.push({ type: 'response.output_item.done', output_index: 1, item: message })
        events.push({ type: 'response.completed', response: result })
        response.writeHead(200, { 'content-type': 'text/event-stream' })
        response.end(events.map((event, sequence_number) => `event: ${String(event.type)}\ndata: ${JSON.stringify({ ...event, sequence_number })}\n\n`).join(''))
      } catch (error) {
        response.writeHead(500, { 'content-type': 'application/json' })
        response.end(JSON.stringify({ error: { message: String(error) } }))
      }
    })
  })
  upstream.listen(0, '127.0.0.1')
  await once(upstream, 'listening')
  const server = spawn(binary, ['--host', '127.0.0.1', '--port', '0', '--data-dir', directory], { cwd: directory, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, NO_COLOR: '1' } })
  const closed = new Promise<void>((resolve) => server.once('close', () => resolve()))
  const ready = new Promise<{ origin: string; token: string }>((resolve, reject) => {
    let logs = ''
    const timeout = setTimeout(() => reject(new Error(`Isolated server readiness timed out: ${logs}`)), 45_000)
    const receive = (chunk: Buffer) => {
      logs += stripVTControlCharacters(chunk.toString())
      const token = /Stravia setup token:\s*(\S+)/.exec(logs)?.[1]
      const port = /(?:Stravia Server listening|startup listener opened)[^\n]*127\.0\.0\.1:(\d+)/.exec(logs)?.[1]
      if (token && port) { clearTimeout(timeout); resolve({ origin: `http://127.0.0.1:${port}`, token }) }
    }
    server.stdout?.on('data', receive)
    server.stderr?.on('data', receive)
    server.once('error', (error) => { clearTimeout(timeout); reject(error) })
    server.once('exit', (code) => { clearTimeout(timeout); reject(new Error(`Server exited before readiness: ${code}`)) })
  })
  const context = await browser.newContext()
  try {
    const { origin, token } = await ready
    await context.addInitScript(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    })
    const page = await context.newPage()
    await page.goto(`${origin}/setup`)
    const api = async (path: string, method = 'GET', body?: unknown): Promise<unknown> => {
      const response = await context.request.fetch(`${origin}/api/v1${path}`, { method, headers: { 'content-type': 'application/json', 'x-stravia-csrf': '1', origin }, data: body })
      if (!response.ok()) throw new Error(`${method} ${path}: ${response.status()} ${await response.text()}`)
      return response.status() === 204 ? undefined : record(await response.json()).data
    }
    await api('/setup/claim', 'POST', { token })
    await api('/setup/complete', 'POST', { database: { backend: 'sqlite' }, username: 'admin', password: 'isolated console password', client_base_url: origin })
    await page.goto(`${origin}/login`)
    await page.locator('#admin-username').fill('admin')
    await page.locator('input[autocomplete="current-password"]').fill('isolated console password')
    await page.locator('button[type="submit"]').click()
    await expect(page).toHaveURL(`${origin}/`)
    const address = upstream.address()
    if (!address || typeof address === 'string') throw new Error('Local Responses provider has no TCP address')
    const serviceId = identifier(await api('/providers', 'POST', {
      name: 'isolated Responses provider', source: { type: 'custom', vendor: 'custom', channel: 'default', protocol: 'open-responses', base_url: `http://127.0.0.1:${address.port}` }, credential: { type: 'api_key', value: 'synthetic-only' }, vendor_options: {},
    }))
    await api(`/providers/${serviceId}/models`, 'POST', { model_id: 'synthetic-reasoner', metadata: { name: 'Synthetic reasoner' } })
    const modelId = identifier(await api('/models', 'POST', { model_id: 'browser-reasoner', display_name: 'Browser reasoner', targets: [{ provider_id: serviceId, model: 'synthetic-reasoner' }] }))
    const selectedKeyId = identifier(await api('/api-keys', 'POST', { name: 'Selected console ownership', model_ids: [modelId] }))
    const otherKeyId = identifier(await api('/api-keys', 'POST', { name: 'Unselected ownership', model_ids: [modelId] }))
    await page.reload()
    await page.getByRole('button', { name: 'API Key', exact: true }).click()
    await page.getByRole('option', { name: 'Selected console ownership', exact: true }).click()
    await page.getByRole('textbox', { name: 'Message', exact: true }).fill('First real question')
    const firstResponse = page.waitForResponse((response) => new URL(response.url()).pathname === '/v1/responses')
    await page.getByRole('button', { name: 'Send', exact: true }).click()
    const received = await firstResponse
    const firstWire = await received.text()
    expect(received.status(), received.status() === 200 ? undefined : firstWire).toBe(200)
    await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('First real gateway answer')
    await expect(page.getByRole('article', { name: 'Assistant response' })).not.toContainText('synthetic-opaque-reasoning')
    await expect(page.getByRole('button', { name: 'Stop', exact: true })).toHaveCount(0)
    const terminal = firstWire.split('\n')
      .filter((line) => line.startsWith('data:') && line.slice(5).trim() !== '[DONE]')
      .map((line) => record(JSON.parse(line.slice(5))))
      .find((event) => event.type === 'response.completed')
    if (!terminal) throw new Error('Gateway omitted the completed response event')
    const firstOutput = record(terminal.response).output
    if (!Array.isArray(firstOutput)) throw new Error('Gateway completed response omitted output items')
    expect(firstOutput.some((item) => record(item).type === 'reasoning')).toBe(true)
    await page.reload()
    await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('First real gateway answer')
    const secondRequest = page.waitForRequest((request) => new URL(request.url()).pathname === '/v1/responses')
    await page.getByRole('textbox', { name: 'Message', exact: true }).fill('Second real question')
    await page.getByRole('button', { name: 'Send', exact: true }).click()
    const replay = record((await secondRequest).postDataJSON())
    expect(replay.input).toEqual([
      { role: 'user', content: 'First real question' },
      ...firstOutput,
      { role: 'user', content: 'Second real question' },
    ])
    expect(replay).not.toHaveProperty('previous_response_id')
    await expect(page.getByRole('article', { name: 'Assistant response' }).last()).toContainText('Second real gateway answer')
    await expect(page.getByRole('article', { name: 'Assistant response' }).last()).not.toContainText('synthetic-opaque-reasoning')
    await expect(page.getByRole('button', { name: 'Stop', exact: true })).toHaveCount(0)
    expect(upstreamRequests).toHaveLength(2)
    const secondUpstream = upstreamRequests[1]
    // Core 可以为同一 Principal/目标将完整回放转换为原生 response 续接。
    if (secondUpstream.previous_response_id) expect(secondUpstream.previous_response_id).toBe('resp_console_1')
    else {
      expect(JSON.stringify(secondUpstream.input)).toContain('synthetic-opaque-reasoning')
      expect(JSON.stringify(secondUpstream.input)).toContain('First real question')
    }
    expect(JSON.stringify(secondUpstream.input)).toContain('Second real question')
    const query = `start_at=${Date.now() - 60_000}&end_at=${Date.now() + 60_000}&limit=30&model=${modelId}`
    await expect.poll(async () => {
      const forest = record(await api(`/observations/interactions?${query}&api_key=${selectedKeyId}`))
      if (!Array.isArray(forest.roots)) throw new Error('Observation forest omitted roots')
      const interactions = forest.roots.flatMap((root) => {
        const interactions = record(root).interactions
        if (!Array.isArray(interactions)) throw new Error('Observation root omitted interactions')
        return interactions
      }).map(record)
      const completed = interactions.find((item) => item.status === 'completed' &&
        typeof item.visible_tail === 'string' && item.visible_tail.includes('First real gateway answer') &&
        item.visible_tail.includes('Second real gateway answer'))
      if (!completed) return null
      const usage = record(completed.usage)
      return {
        attempts: record(usage.coverage).attempt_count,
        inputTokens: usage.input_tokens,
        outputTokens: usage.output_tokens,
      }
    }).toEqual({ attempts: 2, inputTokens: 34, outputTokens: 18 })
    const otherForest = record(await api(`/observations/interactions?${query}&api_key=${otherKeyId}`))
    expect(otherForest.roots).toEqual([])
  } finally {
    try {
      await context.close()
    } finally {
      try {
        upstream.closeAllConnections()
        if (upstream.listening) await new Promise<void>((resolve, reject) => upstream.close((error) => error ? reject(error) : resolve()))
      } finally {
        if (server.exitCode === null) server.kill()
        await closed
        await rm(directory, { recursive: true, force: true })
      }
    }
  }
})
