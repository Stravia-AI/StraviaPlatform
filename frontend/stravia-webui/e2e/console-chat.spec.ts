import { expect, test, type Page } from '@playwright/test'
import { prepareApp } from './prepare-app'

type StreamRequest = { body: { input: unknown[]; model: string }; authorization: string | null; aborted: boolean }
type StreamHarness = {
  requests: StreamRequest[]
  emit: (index: number, event: Record<string, unknown>, close?: boolean) => void
  fail: (index: number, status: number, message: string) => void
}
declare global {
  interface Window {
    consoleStreams: StreamHarness
    chatXss?: boolean
  }
}

const model = {
  id: 'console-model',
  model_id: 'console-model',
  display_name: 'Console model',
  is_enabled: true,
  supported_thinking_levels: ['low', 'high'],
  default_thinking_level: 'low',
  targets: [{ enabled: true, provider_id: 'provider-console' }],
  created_at: '2026-01-01T00:00:00Z',
}
const key = {
  id: 'key-console',
  name: 'Console key',
  key: 'sk-console-test-only',
  is_enabled: true,
  expires_at: null,
  model_ids: ['console-model'],
  rpm_limit: null,
  mcp_access_enabled: false,
  transparent_injection_enabled: false,
  inject_web_search: false,
  inject_media_understanding: false,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}

test.beforeEach(async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 })
})

async function prepareChat(page: Page) {
  await prepareApp(page)
  const catalog = { providers: [{ id: 'provider-console', is_enabled: true }], models: [model], keys: [key] }
  for (const [path, read] of [
    ['providers', () => catalog.providers],
    ['models', () => catalog.models],
    ['api-keys', () => catalog.keys],
  ] as const) {
    await page.route(`**/api/v1/${path}`, (route) => route.fulfill({ json: { data: read() } }))
  }
  // Intercept at the page fetch seam: the preview server deliberately returns
  // 502 for /v1, and Playwright route.fulfill cannot expose a controllable stream.
  await page.addInitScript(() => {
    const originalFetch = window.fetch.bind(window)
    const controllers: ReadableStreamDefaultController<Uint8Array>[] = []
    const encoder = new TextEncoder()
    const requests: StreamRequest[] = []
    const harness: StreamHarness = {
      requests,
      emit(index, event, close = false) {
        controllers[index].enqueue(encoder.encode(`event: ${String(event.type)}\ndata: ${JSON.stringify(event)}\n\n`))
        if (close) controllers[index].close()
      },
      fail(index, status, message) {
        harness.emit(
          index,
          { type: 'response.failed', response: { status: 'failed', error: { status, message } } },
          true,
        )
      },
    }
    window.consoleStreams = harness
    window.fetch = async (input, init) => {
      const url = input instanceof Request ? input.url : String(input)
      if (new URL(url, location.origin).pathname !== '/v1/responses') return originalFetch(input, init)
      if (typeof init?.body !== 'string') throw new Error('Console request body must be JSON text')
      const request: StreamRequest = {
        body: JSON.parse(init.body),
        authorization: new Headers(init.headers).get('authorization'),
        aborted: false,
      }
      const index = requests.push(request) - 1
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          controllers[index] = controller
          init?.signal?.addEventListener(
            'abort',
            () => {
              request.aborted = true
              try {
                controller.error(new DOMException('Aborted', 'AbortError'))
              } catch {
                /* Already closed. */
              }
            },
            { once: true },
          )
        },
      })
      return new Response(body, { headers: { 'content-type': 'text/event-stream' } })
    }
  })
  return catalog
}

async function requests(page: Page) {
  return page.evaluate(() => window.consoleStreams.requests)
}
async function emit(page: Page, index: number, event: Record<string, unknown>, close = false) {
  await page.evaluate(({ index, event, close }) => window.consoleStreams.emit(index, event, close), {
    index,
    event,
    close,
  })
}
async function send(page: Page, text: string, count: number) {
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill(text)
  await page.getByRole('button', { name: 'Send', exact: true }).click()
  await expect.poll(async () => (await requests(page)).length).toBe(count)
  await expect(page).toHaveURL(/conversation=/)
}
function response(text: string) {
  const output: Record<string, unknown>[] = [
    {
      type: 'message',
      id: 'answer',
      role: 'assistant',
      status: 'completed',
      content: [{ type: 'output_text', text, annotations: [] }],
    },
  ]
  return {
    status: 'completed',
    model: 'console-model',
    output,
    usage: { input_tokens: 11, input_tokens_details: { cached_tokens: 4 }, output_tokens: 7 },
  }
}
async function complete(page: Page, index: number, text: string, status = 'completed') {
  await emit(
    page,
    index,
    {
      type: `response.${status}`,
      response: {
        ...response(text),
        status,
        ...(status === 'incomplete' ? { incomplete_details: { reason: 'max_output_tokens' } } : {}),
      },
    },
    true,
  )
  await expect(page.getByRole('button', { name: 'Stop', exact: true })).toHaveCount(0)
}

test('streams safely, copies only visible output and persists authoritative responses across refresh', async ({
  page,
  context,
}) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write'])
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'First persistent question', 1)
  await expect(page.getByRole('article', { name: 'Your message' })).toContainText('First persistent question')
  await emit(page, 0, { type: 'response.output_text.delta', delta: 'Live answer<!-- private unfinished' })
  const answer = page.getByRole('article', { name: 'Assistant response' })
  await expect(answer).toContainText('Live answer')
  await expect(answer).not.toContainText('private unfinished')
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toBeDisabled()
  const text =
    '## Final answer\n\n**Safe**\n\n```ts\nconst result = 42\n```\n<!-- stravia-private-marker -->\n<script>window.chatXss = true</script>'
  const terminal = response(text)
  terminal.output.unshift({
    type: 'reasoning',
    id: 'reason',
    summary: [{ type: 'summary_text', text: 'Checked carefully<!-- hidden reasoning -->' }],
  })
  await emit(page, 0, { type: 'response.completed', response: terminal }, true)
  await expect(answer.getByRole('heading', { name: 'Final answer' })).toBeVisible()
  await expect(answer.locator('pre')).toContainText('const result = 42')
  await expect(answer.getByText('Checked carefully', { exact: true })).not.toBeVisible()
  await answer.getByText('Reasoning', { exact: true }).click()
  await expect(answer.getByText('Checked carefully', { exact: true })).toBeVisible()
  await expect(answer).not.toContainText('hidden reasoning')
  await expect(answer).toContainText('Input: 7 tokens')
  await expect(answer).toContainText('Output: 7 tokens')
  await expect(answer).not.toContainText('stravia-private-marker')
  await expect(page.getByRole('heading', { level: 1 })).toHaveCount(1)
  expect(await page.evaluate(() => window.chatXss)).toBeUndefined()
  await answer.getByRole('button', { name: /Copy/ }).click()
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain('Final answer')
  expect(await page.evaluate(() => navigator.clipboard.readText())).not.toContain('stravia-private-marker')
  await page.reload()
  await expect(answer).toContainText('Final answer')
  await expect(answer).toContainText('Input: 7 tokens')
  await send(page, 'Continue the saved conversation', 1)
  const replay = (await requests(page))[0]
  expect(replay.authorization).toBe('Bearer sk-console-test-only')
  expect(replay.body.input).toEqual(expect.arrayContaining(terminal.output))
  await complete(page, 0, 'Continued successfully')
})

test('legacy history preserves reasoning and replay without presenting unconvertible total input', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/api/v1/auth/state')
  const legacyOutput = await page.evaluate(async () => {
    const firstOutput = [
      {
        type: 'reasoning',
        id: 'legacy-reason',
        encrypted_content: 'legacy-opaque',
        summary: [{ type: 'summary_text', text: 'Legacy readable thought' }],
      },
      { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Legacy first answer' }] },
    ]
    const secondOutput = [
      { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Legacy second answer' }] },
    ]
    await new Promise<void>((resolve, reject) => {
      const request = indexedDB.open('stravia-console-chat', 1)
      request.onupgradeneeded = () => {
        request.result.createObjectStore('conversations', { keyPath: 'id' })
        request.result.createObjectStore('preferences')
      }
      request.onerror = () => reject(request.error ?? new Error('Legacy history database open failed'))
      request.onsuccess = () => {
        const database = request.result
        const transaction = database.transaction(['conversations', 'preferences'], 'readwrite')
        transaction.objectStore('conversations').put({
          id: 'legacy',
          title: 'Legacy conversation',
          apiKeyId: 'key-console',
          apiKeyName: 'Console key',
          selectedModelId: 'console-model',
          thinkingSelection: 'default',
          createdAt: '2026-01-01T00:00:00Z',
          updatedAt: '2026-01-01T00:00:00Z',
          messages: [
            { id: 'user-1', role: 'user', text: 'Legacy first question', createdAt: '2026-01-01T00:00:00Z' },
            {
              id: 'answer-1',
              role: 'assistant',
              status: 'completed',
              routeId: 'console-model',
              thinkingLevel: 'default',
              createdAt: '2026-01-01T00:00:00Z',
              outputItems: firstOutput,
              usage: { inputTokens: 99, outputTokens: 7 },
            },
            { id: 'user-2', role: 'user', text: 'Legacy second question', createdAt: '2026-01-01T00:00:00Z' },
            {
              id: 'answer-2',
              role: 'assistant',
              status: 'completed',
              routeId: 'console-model',
              thinkingLevel: 'default',
              createdAt: '2026-01-01T00:00:00Z',
              outputItems: secondOutput,
              usage: { inputTokens: 0, outputTokens: 0 },
            },
          ],
        })
        transaction.objectStore('preferences').put({ apiKeyId: 'key-console', modelId: 'console-model' }, 'selection')
        transaction.oncomplete = () => {
          database.close()
          resolve()
        }
        transaction.onabort = () => {
          database.close()
          reject(transaction.error ?? new Error('Legacy history seed transaction aborted'))
        }
      }
    })
    return { firstOutput, secondOutput }
  })
  await page.goto('/?conversation=legacy')
  const answers = page.getByRole('article', { name: 'Assistant response' })
  await expect(answers).toHaveCount(2)
  await expect(answers.first()).toContainText('Legacy first answer')
  await expect(answers.last()).toContainText('Legacy second answer')
  await expect(answers.first()).toContainText('Output: 7 tokens')
  await expect(answers.last()).toContainText('Output: 0 tokens')
  await expect(answers.first()).not.toContainText('Input:')
  await expect(answers.last()).not.toContainText('Input:')
  await answers.first().getByText('Reasoning', { exact: true }).click()
  await expect(answers.first()).toContainText('Legacy readable thought')
  await page.reload()
  await expect(answers.first()).not.toContainText('Input:')
  await send(page, 'Continue legacy history', 1)
  expect((await requests(page))[0].body.input).toEqual([
    { role: 'user', content: 'Legacy first question' },
    ...legacyOutput.firstOutput,
    { role: 'user', content: 'Legacy second question' },
    ...legacyOutput.secondOutput,
    { role: 'user', content: 'Continue legacy history' },
  ])
  await complete(page, 0, 'Continued legacy answer')
  await expect(answers.last()).toContainText('Input: 7 tokens')
  await page.reload()
  await expect(answers.last()).toContainText('Input: 7 tokens')
  await expect(answers.first()).not.toContainText('Input:')
})

test('stops live output and refresh interrupts another turn without losing received content', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'Stop this turn', 1)
  await emit(page, 0, { type: 'response.output_text.delta', delta: 'Partial retained answer' })
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('Partial retained answer')
  await page.getByRole('button', { name: 'Stop', exact: true }).click()
  await expect.poll(async () => (await requests(page))[0].aborted).toBe(true)
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText(/Stopped/)
  await send(page, 'Interrupted by refresh', 2)
  expect(JSON.stringify((await requests(page))[1].body.input)).toContain('Partial retained answer')
  await emit(page, 1, { type: 'response.output_text.delta', delta: 'Saved before refresh' })
  await expect(page.getByRole('article', { name: 'Assistant response' }).last()).toContainText('Saved before refresh')
  await page.reload()
  await expect(page.getByRole('article', { name: 'Assistant response' }).last()).toContainText('Saved before refresh')
  await expect(page.getByRole('article', { name: 'Assistant response' }).last()).toContainText(/Stopped/)
})

test('failure requires manual retry and incomplete output remains available for continuation', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'Retry the same request', 1)
  await page.evaluate(() => window.consoleStreams.fail(0, 429, 'Synthetic request limit reached'))
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText(
    'Synthetic request limit reached',
  )
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
  expect((await requests(page)).length).toBe(1)
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect.poll(async () => (await requests(page)).length).toBe(2)
  expect((await requests(page))[1].body).toEqual((await requests(page))[0].body)
  await complete(page, 1, 'Truncated useful answer', 'incomplete')
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText(/Incomplete/)
  await send(page, 'Continue after truncation', 3)
  expect(JSON.stringify((await requests(page))[2].body.input)).toContain('Truncated useful answer')
  expect(JSON.stringify((await requests(page))[2].body.input)).not.toContain('Synthetic request limit reached')
  await complete(page, 2, 'Continuation')
})

test('generation survives route changes and concurrent conversations appear in recent navigation', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'Background first conversation', 1)
  const first = page.url()
  await page.getByRole('link', { name: 'Models', exact: true }).first().click()
  await expect(page).toHaveURL(/\/models/)
  const recent = page.getByRole('link').filter({ hasText: 'Background first conversation' })
  await expect(recent.getByRole('status')).toHaveAccessibleName('Generating…')
  await emit(page, 0, { type: 'response.output_text.delta', delta: 'Background output' })
  await page.getByRole('link', { name: 'Chat', exact: true }).first().click()
  await send(page, 'Parallel second conversation', 2)
  await emit(page, 0, { type: 'response.completed', response: response('First finished in background') }, true)
  await complete(page, 1, 'Second finished')
  await recent.click()
  await expect(page).toHaveURL(first)
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('First finished in background')
  await page.reload()
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('First finished in background')
})

test('management session expiry does not interrupt Key-authenticated generation or erase local history', async ({
  page,
}) => {
  await prepareChat(page)
  let expired = false
  await page.route('**/api/v1/auth/state', (route) =>
    route.fulfill({
      json: {
        mode: 'server',
        authenticated: !expired,
        setup_authorized: false,
        username: expired ? null : 'playwright-admin',
      },
    }),
  )
  await page.route('**/api/v1/auth/refresh', (route) =>
    route.fulfill({ status: 401, json: { error: 'Session expired' } }),
  )
  await page.route('**/api/v1/auth/login', async (route) => {
    expired = false
    await route.fulfill({
      json: {
        data: {
          username: 'playwright-admin',
          access_expires_at: Date.now() + 60_000,
          session_expires_at: Date.now() + 3_600_000,
        },
      },
    })
  })
  await page.route('**/api/v1/models', async (route) => {
    if (expired) await route.fulfill({ status: 401, json: { error: 'Session expired' } })
    else await route.fallback()
  })
  await page.goto('/')
  await send(page, 'Survives management login', 1)
  const conversationAddress = page.url()
  expired = true
  await page.getByRole('link', { name: 'Models', exact: true }).first().click()
  await expect(page).toHaveURL(/\/login/)
  expect((await requests(page))[0].aborted).toBe(false)
  await emit(page, 0, { type: 'response.completed', response: response('Completed while signed out') }, true)
  await page.locator('#admin-username').fill('playwright-admin')
  await page.locator('input[autocomplete="current-password"]').fill('test-only-password')
  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(/\/$/)
  await page.goto(conversationAddress)
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('Completed while signed out')
  await page.reload()
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('Completed while signed out')
})

test('history searches message content, renames, deletes and confirms exact clear count', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  for (let index = 0; index < 6; index++) {
    if (index) await page.getByRole('button', { name: 'New conversation', exact: true }).click()
    await send(page, `History title ${index}`, index + 1)
    await complete(page, index, `Unique body needle ${index}`)
  }
  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  await expect(navigation.getByRole('link', { name: /^History title/ })).toHaveCount(5)
  await expect(navigation.getByRole('link', { name: 'History title 5', exact: true })).toHaveAttribute(
    'aria-current',
    'page',
  )
  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  await expect(navigation.getByRole('link', { name: /^History title/ })).toHaveCount(0)
  await expect(navigation.getByRole('link', { name: 'Chat', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Expand navigation' }).click()
  await page.getByRole('link', { name: 'All conversations', exact: true }).first().click()
  const search = page.getByRole('searchbox', { name: 'Search conversations' })
  await search.fill('needle 3')
  await expect(page.getByRole('main').getByRole('link', { name: 'History title 3', exact: true })).toBeVisible()
  await expect(page.getByRole('main').getByRole('link', { name: 'History title 2', exact: true })).toHaveCount(0)
  await page.getByRole('main').getByRole('button', { name: 'Rename', exact: true }).click()
  const rename = page.getByRole('dialog', { name: 'Rename conversation' })
  await rename.getByRole('textbox').fill('Renamed browser conversation')
  await rename.getByRole('button', { name: /Save|Rename/, exact: true }).click()
  await search.fill('Renamed')
  await expect(page.getByRole('main').getByRole('link', { name: 'Renamed browser conversation' })).toBeVisible()
  await page.getByRole('main').getByRole('button', { name: 'Delete', exact: true }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Delete', exact: true }).click()
  await search.fill('')
  await page.getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  const clear = page.getByRole('alertdialog', { name: 'Clear all conversations?' })
  await expect(clear).toContainText('5')
  await expect(clear).toContainText(/cannot be undone|cannot be recovered|permanent/i)
  await clear.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(clear).toHaveCount(0)
  await expect(page.getByRole('main').getByRole('link', { name: /^History title/ })).toHaveCount(5)
  await page.getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  await page.goto('/conversations')
  await page.reload()
  await expect(page.getByRole('main').getByRole('link', { name: 'Start new conversation' })).toBeVisible()
})

test('invalidated key makes persisted conversation read-only and offers a fresh identity', async ({ page }) => {
  const catalog = await prepareChat(page)
  await page.goto('/')
  await send(page, 'Locked key conversation', 1)
  await complete(page, 0, 'Answer remains readable')
  catalog.keys = [
    { ...key, is_enabled: false },
    { ...key, id: 'replacement-key', name: 'Replacement key' },
  ]
  await page.reload()
  await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('Answer remains readable')
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveCount(0)
  const address = page.url()
  await page.goto('/conversations')
  await expect(page.getByRole('table').getByRole('row').filter({ hasText: 'Locked key conversation' })).toContainText(
    'Read-only',
  )
  await page.goto(address)
  await page.getByRole('button', { name: 'New conversation with another Key' }).click()
  await expect(page.getByRole('button', { name: 'API Key', exact: true })).toContainText('Replacement key')
  await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toBeEditable()
})

for (const configuration of [
  { name: 'missing services', providers: [], models: [model], keys: [key], destination: '/providers' },
  {
    name: 'disabled services',
    providers: [{ id: 'p', is_enabled: false }],
    models: [model],
    keys: [key],
    destination: '/providers',
  },
  {
    name: 'missing models',
    providers: [{ id: 'provider-console', is_enabled: true }],
    models: [],
    keys: [key],
    destination: '/models',
  },
  {
    name: 'disabled models',
    providers: [{ id: 'provider-console', is_enabled: true }],
    models: [{ ...model, is_enabled: false }],
    keys: [key],
    destination: '/models',
  },
  {
    name: 'missing keys',
    providers: [{ id: 'provider-console', is_enabled: true }],
    models: [model],
    keys: [],
    destination: '/api-keys',
  },
  {
    name: 'unavailable keys',
    providers: [{ id: 'provider-console', is_enabled: true }],
    models: [model],
    keys: [{ ...key, expires_at: '2000-01-01T00:00:00Z' }],
    destination: '/api-keys',
  },
]) {
  test(`guides ${configuration.name} to configuration instead of accepting input`, async ({ page }) => {
    const catalog = await prepareChat(page)
    Object.assign(catalog, configuration)
    await page.goto('/')
    await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveCount(0)
    await page.getByRole('main').getByRole('link').filter({ hasNotText: 'All conversations' }).first().click()
    await expect(page).toHaveURL(new RegExp(`${configuration.destination}(?:[?#].*)?$`))
  })
}

test('missing addresses recover and Chinese mobile users can send and stop from the keyboard', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/?conversation=not-in-this-browser')
  await expect(page.getByRole('heading', { name: 'Conversation not found' })).toBeVisible()
  await page.getByRole('button', { name: 'Start new conversation' }).click()
  await page.getByRole('link', { name: 'Settings', exact: true }).first().click()
  await page.getByRole('button', { name: 'Language', exact: true }).click()
  await page.getByRole('option', { name: '简体中文' }).click()
  await page.getByRole('link', { name: '对话', exact: true }).first().click()
  await page.setViewportSize({ width: 390, height: 844 })
  await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
  const composer = page.getByRole('textbox', { name: '消息', exact: true })
  await composer.focus()
  await page.keyboard.type('Keyboard mobile question')
  await page.getByRole('button', { name: '发送', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect.poll(async () => (await requests(page)).length).toBe(1)
  await emit(page, 0, { type: 'response.output_text.delta', delta: '移动端回答' })
  await expect(page.getByRole('article', { name: '助手回答' })).toContainText('移动端回答')
  await page.getByRole('button', { name: '停止', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect.poll(async () => (await requests(page))[0].aborted).toBe(true)
  await page.getByRole('button', { name: /Open navigation|打开导航/ }).click()
  const navigation = page.getByRole('dialog')
  await expect(navigation.getByRole('link').filter({ hasText: 'Keyboard mobile question' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(navigation).toHaveCount(0)
})

test('keyboard selection determines the identity, model and reasoning used for a turn', async ({ page }) => {
  const catalog = await prepareChat(page)
  catalog.keys = [key, { ...key, id: 'key-second', name: 'Second key', key: 'sk-second-only', model_ids: [] }]
  catalog.models = [model, { ...model, id: 'second-model', model_id: 'second-model', display_name: 'Second model' }]
  await page.goto('/')
  const keySelector = page.getByRole('button', { name: 'API Key', exact: true })
  await expect(keySelector).toContainText('Choose an API Key')
  await keySelector.focus()
  await page.keyboard.press('Enter')
  await page.keyboard.press('End')
  await page.keyboard.press('Enter')
  await expect(keySelector).toContainText('Second key')
  const modelSelector = page.getByRole('button', { name: 'Model', exact: true })
  await modelSelector.focus()
  await page.keyboard.press('Enter')
  await page.keyboard.press('End')
  await page.keyboard.press('Enter')
  await expect(modelSelector).toContainText('Second model')
  await page.getByRole('button', { name: 'Reasoning effort', exact: true }).focus()
  await page.keyboard.press('Enter')
  await page.keyboard.press('End')
  await page.keyboard.press('Enter')
  await page.getByRole('textbox', { name: 'Message', exact: true }).focus()
  await page.keyboard.type('Selected with keyboard')
  await page.keyboard.press('ControlOrMeta+Enter')
  await expect.poll(async () => (await requests(page)).length).toBe(1)
  const request = (await requests(page))[0]
  expect(request.authorization).toBe('Bearer sk-second-only')
  expect(request.body.model).toBe('second-model')
  await page.getByRole('button', { name: 'Stop', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect.poll(async () => (await requests(page))[0].aborted).toBe(true)
  await page.getByRole('button', { name: 'New conversation', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/$/)
  const recent = page.getByRole('link', { name: 'Selected with keyboard', exact: true })
  await recent.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('article', { name: 'Your message' })).toContainText('Selected with keyboard')
})

test.describe('UTC expiry boundaries', () => {
  test.use({ timezoneId: 'Asia/Shanghai' })
  test('idle conversation becomes read-only at its Key expiry boundary', async ({ page }) => {
    await page.clock.install({ time: new Date('2026-01-01T00:00:00Z') })
    const catalog = await prepareChat(page)
    catalog.keys = [{ ...key, expires_at: '2026-01-01 00:00:10' }]
    await page.goto('/')
    await send(page, 'Before expiry', 1)
    await complete(page, 0, 'Preserved answer')
    await page.clock.fastForward(11_000)
    await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'New conversation with another Key' })).toBeVisible()
    await expect(page.getByRole('article', { name: 'Assistant response' })).toContainText('Preserved answer')
  })
})

test('removed failed model offers regeneration with its available replacement', async ({ page }) => {
  const catalog = await prepareChat(page)
  await page.goto('/')
  await send(page, 'Original model question', 1)
  catalog.models = [
    { ...model, id: 'replacement-model', model_id: 'replacement-model', display_name: 'Replacement model' },
  ]
  catalog.keys = [{ ...key, model_ids: ['replacement-model'] }]
  await emit(page, 0, { type: 'error', error: { code: 'STRAVIA_NOT_FOUND', message: 'Model was removed' } }, true)
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeDisabled()
  await expect(page.getByRole('alert')).toContainText(/regenerate/)
  await page.getByRole('button', { name: 'Regenerate', exact: true }).click()
  await expect.poll(async () => (await requests(page)).length).toBe(2)
  expect((await requests(page))[1].body.model).toBe('replacement-model')
  await complete(page, 1, 'Replacement answer')
})

test('history directory errors remain distinct from a deleted Key', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'Readable despite directory error', 1)
  await complete(page, 0, 'Persisted readable answer')
  let unavailable = true
  await page.route('**/api/v1/api-keys', async (route) => {
    if (unavailable) await route.fulfill({ status: 500, json: { error: 'Key directory unavailable' } })
    else await route.fallback()
  })
  await page.goto('/conversations')
  await page.reload()
  await expect(page.getByRole('alert')).toContainText('Key directory unavailable')
  await expect(page.getByRole('main')).not.toContainText('Read-only')
  await expect(
    page.getByRole('main').getByRole('link', { name: 'Readable despite directory error', exact: true }),
  ).toBeVisible()
  unavailable = false
  await page.getByRole('button', { name: 'Refresh configuration', exact: true }).click()
  await expect(page.getByRole('alert')).toHaveCount(0)
})

test('failed IndexedDB clear keeps history and the confirmation retryable', async ({ page }) => {
  await prepareChat(page)
  await page.goto('/')
  await send(page, 'Keep until clear succeeds', 1)
  await complete(page, 0, 'Saved answer')
  await page.goto('/conversations')
  await page.evaluate(() => {
    const clear = Reflect.get(IDBObjectStore.prototype, 'clear')
    IDBObjectStore.prototype.clear = function () {
      if (this.name === 'conversations') {
        IDBObjectStore.prototype.clear = clear
        throw new DOMException('Local deletion unavailable', 'UnknownError')
      }
      return clear.call(this)
    }
  })
  await page.getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  const confirmation = page.getByRole('alertdialog')
  await confirmation.getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  await expect(confirmation.getByRole('alert')).toContainText('Local deletion unavailable')
  await expect(
    page.getByRole('main').getByRole('link', { name: 'Keep until clear succeeds', exact: true }),
  ).toBeVisible()
  await confirmation.getByRole('button', { name: 'Clear all conversations', exact: true }).click()
  await expect(confirmation).toHaveCount(0)
  await page.reload()
  await expect(page.getByRole('main').getByRole('link', { name: 'Start new conversation', exact: true })).toBeVisible()
})
