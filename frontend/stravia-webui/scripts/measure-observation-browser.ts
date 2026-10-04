// Paired measurement consumer: real Chromium, no intercepted management APIs.
// Private startup/session values are read from stdin and never printed/exported.
import { chromium } from '@playwright/test'
import { createInterface } from 'node:readline'
import { mkdir, writeFile } from 'node:fs/promises'
import { dirname } from 'node:path'

interface DisplayMeasurement {
  first_body_at: number | null
  last_body_change_at: number | null
  longest_body_chars: number
}
declare global {
  interface Window {
    observationMeasurement?: DisplayMeasurement
  }
}
const lines = createInterface({ input: process.stdin })[Symbol.asyncIterator]()
async function command() {
  const line = await lines.next()
  if (line.done) throw new Error('Measurement controller disconnected')
  return JSON.parse(line.value)
}
const config = await command()
const browser = await chromium.launch({ headless: true })
const contexts = []
const pages = []
const sessions = []
const pageErrors: string[] = []
const requests = new Map<
  string,
  { path: string; method: string; start: number; bytes: number; status?: number; duration_ms?: number }
>()
const sse = [] as Array<{ event: string; bytes: number; received_at: number; sequence?: number; revision?: number }>
const sseCaptureErrors: string[] = []
try {
  for (let index = 0; index < config.viewers; index++) {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } })
    contexts.push(context)
    await context.addInitScript(() => {
      localStorage.setItem('stravia-locale', 'en-US')
      localStorage.setItem('stravia-sidebar-state', 'expanded')
      const state: DisplayMeasurement = { first_body_at: null, last_body_change_at: null, longest_body_chars: 0 }
      window.observationMeasurement = state
      let previous = ''
      const update = () => {
        const body = document.querySelector('aside[aria-label="Observation details"] [role="log"]')?.textContent ?? ''
        if (body !== previous && (body.includes('A文🙂') || body.includes('B文🙂'))) {
          state.first_body_at ??= Date.now()
          state.last_body_change_at = Date.now()
          state.longest_body_chars = Math.max(state.longest_body_chars, body.length)
        }
        previous = body
      }
      document.addEventListener(
        'DOMContentLoaded',
        () => {
          new MutationObserver(update).observe(document.body, { subtree: true, childList: true, characterData: true })
          update()
        },
        { once: true },
      )
    })
    const page = await context.newPage()
    page.on('pageerror', (error) => pageErrors.push(error.message))
    pages.push(page)
    await page.goto(`${config.origin}/login`)
    await page.locator('#admin-username').fill('admin')
    await page.locator('input[autocomplete="current-password"]').fill(config.password)
    await page.locator('button[type="submit"]').click()
    await page.waitForURL(`${config.origin}/`)
    const cdp = await context.newCDPSession(page)
    const liveBuffers = new Map<string, { decoder: TextDecoder; text: string; ready: boolean; pending: string[] }>()
    const decodeSse = (requestId: string, encoded: string) => {
      const state = liveBuffers.get(requestId)
      if (!state) return
      state.text += state.decoder.decode(Buffer.from(encoded, 'base64'), { stream: true })
      const frames = state.text.split(/\r?\n\r?\n/)
      state.text = frames.pop() ?? ''
      for (const frame of frames) {
        const event =
          frame
            .split(/\r?\n/)
            .find((line) => line.startsWith('event:'))
            ?.slice(6)
            .trim() ?? ''
        const data = frame
          .split(/\r?\n/)
          .filter((line) => line.startsWith('data:'))
          .map((line) => line.slice(5).trim())
          .join('\n')
        if (!data) continue
        const value = JSON.parse(data)
        sse.push({
          event,
          bytes: Buffer.byteLength(data),
          received_at: Date.now(),
          sequence: value.sequence,
          revision: value.revision ?? value.block?.revision,
        })
      }
    }
    await cdp.send('Network.enable')
    await cdp.send('Performance.enable')
    if (config.scenario === 'slow-consumer') await cdp.send('Emulation.setCPUThrottlingRate', { rate: 4 })
    cdp.on('Network.requestWillBeSent', (event) => {
      const url = new URL(event.request.url)
      if (!url.pathname.includes('/observations/')) return
      requests.set(`${index}:${event.requestId}`, {
        path: url.pathname,
        method: event.request.method,
        start: event.timestamp,
        bytes: 0,
      })
    })
    cdp.on('Network.responseReceived', (event) => {
      const request = requests.get(`${index}:${event.requestId}`)
      if (request) request.status = event.response.status
      if (request && event.response.mimeType === 'text/event-stream') {
        const state = { decoder: new TextDecoder(), text: '', ready: false, pending: [] as string[] }
        liveBuffers.set(event.requestId, state)
        cdp
          .send('Network.streamResourceContent', { requestId: event.requestId })
          .then((result) => {
            if (result.bufferedData) decodeSse(event.requestId, result.bufferedData)
            state.ready = true
            for (const encoded of state.pending) decodeSse(event.requestId, encoded)
            state.pending = []
          })
          .catch((error: Error) => {
            sseCaptureErrors.push(error.message)
          })
      }
    })
    cdp.on('Network.dataReceived', (event) => {
      const request = requests.get(`${index}:${event.requestId}`)
      if (request) request.bytes += event.dataLength
      if (event.data) {
        const state = liveBuffers.get(event.requestId)
        if (state?.ready) decodeSse(event.requestId, event.data)
        else state?.pending.push(event.data)
      }
    })
    cdp.on('Network.loadingFinished', (event) => {
      const request = requests.get(`${index}:${event.requestId}`)
      if (request) request.duration_ms = (event.timestamp - request.start) * 1000
    })
    await page.goto(`${config.origin}/logs`)
    if (config.scenario === 'failure-page')
      await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    else await page.getByRole('tab', { name: 'Interaction Chains', exact: true }).waitFor()
    sessions.push({ cdp, before: await cdp.send('Performance.getMetrics') })
  }
  console.log('ready')
  let message = await command()
  const selections = message
  if (message.interactions && !['no-detail', 'failure-page'].includes(config.scenario)) {
    for (const [index, page] of pages.entries()) {
      const id = message.interactions[index % message.interactions.length]
      // Actual selection through the supported deep link, not a synthetic API client.
      await page.goto(`${config.origin}/logs?interaction=${id}`)
      await page.getByRole('complementary', { name: 'Observation details' }).waitFor()
    }
  }
  message = await command()
  if (!message.done) throw new Error('Missing workload completion signal')
  if (config.scenario === 'reconnect') await pages[0].reload()
  // Wait on observed terminal UI state, not a fixed post-load sleep.
  if (config.scenario === 'failure-page')
    await pages[0].getByRole('table').getByRole('row').filter({ hasText: 'measurement' }).first().waitFor()
  else if (config.scenario === 'no-detail')
    await pages[0].getByRole('button', { name: 'measurement display, Completed', exact: true }).first().waitFor()
  else if (config.scenario !== 'no-detail') {
    for (const [index, page] of pages.entries()) {
      const marker = selections.markers[index % selections.markers.length]
      await page.waitForFunction(
        (expected) =>
          document
            .querySelector('aside[aria-label="Observation details"] [role="log"]')
            ?.textContent?.includes(expected),
        (marker + '文🙂'.repeat(40)).repeat(80),
        { timeout: 15000 },
      )
    }
  }
  const viewers = []
  for (const [index, page] of pages.entries()) {
    const after = await sessions[index].cdp.send('Performance.getMetrics')
    const previous = new Map(
      sessions[index].before.metrics.map((metric: { name: string; value: number }) => [metric.name, metric.value]),
    )
    const metrics = Object.fromEntries(
      after.metrics
        .filter((metric: { name: string }) =>
          [
            'TaskDuration',
            'ScriptDuration',
            'LayoutDuration',
            'RecalcStyleDuration',
            'JSHeapUsedSize',
            'Nodes',
          ].includes(metric.name),
        )
        .map((metric: { name: string; value: number }) => [
          metric.name,
          { value: metric.value, delta: metric.value - Number(previous.get(metric.name) ?? 0) },
        ]),
    )
    const display = await page.evaluate(() => window.observationMeasurement)
    const screenshot = `${config.output}.viewer-${index}.png`
    await mkdir(dirname(screenshot), { recursive: true })
    await page.screenshot({ path: screenshot, fullPage: true })
    viewers.push({
      metrics,
      display,
      screenshot,
      rendered_nodes: await page.getByRole('button', { name: /^measurement display,/ }).count(),
    })
  }
  const result = {
    scenario: config.scenario,
    requests: [...requests.values()].map((request) => ({
      path: request.path,
      method: request.method,
      bytes: request.bytes,
      status: request.status,
      duration_ms: request.duration_ms,
    })),
    sse,
    sse_capture_errors: sseCaptureErrors,
    page_errors: pageErrors,
    viewers,
    limits: [
      'CDP dataLength measures decoded HTTP body bytes, not TCP overhead',
      'Performance durations are Chromium main-thread service CPU; SSE connection duration is intentionally unbounded',
      'Local-host wall timestamps are measured actual display/receipt instants, not the active scheduler budget',
    ],
  }
  await writeFile(config.output, JSON.stringify(result, null, 2))
  console.log('complete')
} catch (error) {
  await mkdir(dirname(config.output), { recursive: true })
  for (const [index, page] of pages.entries()) {
    await page.screenshot({ path: `${config.output}.error-${index}.png`, fullPage: true })
  }
  await writeFile(
    `${config.output}.failure.json`,
    JSON.stringify({
      page_errors: pageErrors,
      locations: pages.map((page) => new URL(page.url()).pathname),
      sse_capture_errors: sseCaptureErrors,
    }),
  )
  if (error instanceof Error) {
    error.message = error.message.replaceAll(config.password, '[redacted]')
    if (error.stack) error.stack = error.stack.replaceAll(config.password, '[redacted]')
  }
  throw error
} finally {
  for (const context of contexts) await context.close()
  await browser.close()
}
