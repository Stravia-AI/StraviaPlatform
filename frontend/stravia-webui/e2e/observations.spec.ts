import { expect, test, type Locator, type Page } from '@playwright/test'

import type {
  ConfirmedUsage,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  ObservationEvent,
  RunDetail,
} from '../src/lib/types'
import { prepareApp } from './prepare-app'

const DAY = 86_400_000
const startedAt = Date.UTC(2026, 8, 6, 8, 0, 0)
const routeIds = {
  atlas: '11111111-1111-4111-8111-111111111111',
  boreal: '22222222-2222-4222-8222-222222222222',
  cinder: '33333333-3333-4333-8333-333333333333',
  delta: '44444444-4444-4444-8444-444444444444',
  ember: '55555555-5555-4555-8555-555555555555',
} as const
const statusLabels: Record<string, string> = {
  completed: 'Completed',
  interrupted: 'Interrupted',
  running: 'Running',
  waiting_client: 'Waiting for client',
}

const usage: ConfirmedUsage = {
  input_tokens: 1_240,
  output_tokens: 86,
  cache_read_tokens: 320,
  cache_write_tokens: null,
  reasoning_tokens: 44,
}

function interaction(
  id: string,
  rootId: string,
  parentId: string | null,
  model: string,
  routeId: string,
  status: string,
  offset: number,
): InteractionSummary {
  return {
    id,
    root_id: rootId,
    parent_interaction_id: parentId,
    generation_root_id: rootId,
    first_route_id: routeId,
    first_model_display_name: model,
    status,
    started_at: startedAt + offset,
    last_active_at: startedAt + offset + 10_000,
    visible_tail: `${model} client-visible answer`,
    usage,
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 10,
  }
}

function runFor(item: InteractionSummary): RunDetail {
  const event: ObservationEvent = {
    sequence: item.last_event_sequence,
    occurred_at: item.last_active_at,
    interaction_id: item.id,
    run_id: `run-${item.id}`,
    rejection_id: null,
    kind: item.status === 'running' ? 'model_turn_started' : 'delivery_terminal',
    payload: item.status === 'running' ? { model_turn_id: `turn-${item.id}` } : { delivered: true },
  }
  return {
    id: `run-${item.id}`,
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: item.first_route_id,
    model_display_name: item.first_model_display_name,
    ingress_protocol: 'openai-responses',
    status: item.status,
    terminal_reason: item.status === 'running' || item.status === 'waiting_client' ? null : 'completed',
    user_interrupted: false,
    debug_enabled: false,
    client_output_committed: item.status === 'completed',
    started_at: item.started_at,
    finished_at: item.status === 'completed' ? item.last_active_at : null,
    usage: item.usage,
    events: [event],
    trace: null,
    debug_events: [],
  }
}

interface ObservationFixture {
  emit(event: ObservationEvent): void
  releaseRemainingRoots(): void
  forestRequests: URL[]
  debugWrites: Array<{ enabled: boolean; confirmed: boolean }>
}

async function installObservationFixture(page: Page, holdRemainingRoots = false): Promise<ObservationFixture> {
  let releaseRemainingRoots!: () => void
  const remainingRootsReady = new Promise<void>((resolve) => {
    releaseRemainingRoots = resolve
  })
  if (!holdRemainingRoots) releaseRemainingRoots()
  const atlas = interaction('interaction-atlas', 'root-a', null, 'Atlas', routeIds.atlas, 'completed', 0)
  const boreal = interaction(
    'interaction-boreal',
    'root-a',
    atlas.id,
    'Boreal',
    routeIds.boreal,
    'waiting_client',
    60_000,
  )
  const cinder = interaction('interaction-cinder', 'root-a', atlas.id, 'Cinder', routeIds.cinder, 'running', 120_000)
  const contextSiblings = ['Context One', 'Context Two', 'Context Three', 'Context Four', 'Context Five'].map(
    (model, index) =>
      interaction(
        `interaction-context-${index + 1}`,
        'root-a',
        atlas.id,
        model,
        routeIds.atlas,
        'completed',
        10_000 + index * 5_000,
      ),
  )
  const delta = interaction('interaction-delta', 'root-b', null, 'Delta', routeIds.delta, 'completed', 180_000)
  const ember = interaction('interaction-ember', 'root-c', null, 'Ember', routeIds.ember, 'completed', 240_000)
  const roots: ForestRoot[] = [
    { id: 'root-a', last_active_at: cinder.last_active_at, interactions: [atlas, boreal, cinder, ...contextSiblings] },
    { id: 'root-b', last_active_at: delta.last_active_at, interactions: [delta] },
    { id: 'root-c', last_active_at: ember.last_active_at, interactions: [ember] },
  ]
  const forestRequests: URL[] = []
  const debugWrites: Array<{ enabled: boolean; confirmed: boolean }> = []
  const streamEvents: ObservationEvent[] = []
  let debugEnabled = false
  let snapshotSequence = 10

  const rootFor = (id: string, status?: string | null): ForestRoot => {
    const root = roots.find((candidate) => candidate.interactions.some((item) => item.id === id))!
    return {
      ...root,
      interactions: root.interactions.map((item) => ({ ...item, matched: !status || item.status === status })),
    }
  }

  await page.route('**/api/v1/observations/**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')

    if (path === '/observations/events') {
      const event = streamEvents.shift()
      const body = event ? `id: ${event.sequence}\nevent: observation\ndata: ${JSON.stringify(event)}\n\n` : ''
      await route.fulfill({ contentType: 'text/event-stream', body })
      return
    }

    if (path === '/observations/debug') {
      if (request.method() === 'PUT') {
        const body = request.postDataJSON() as { enabled: boolean; confirmed: boolean }
        debugWrites.push(body)
        debugEnabled = body.enabled
      }
      await route.fulfill({
        json: {
          data: {
            enabled: debugEnabled,
            run_limit_bytes: 67_108_864,
            total_limit_bytes: 2_147_483_648,
            retained_bytes: 0,
            partial_trace_count: 0,
            retention_days: 7,
          },
        },
      })
      return
    }

    const detailMatch = path.match(/^\/observations\/interactions\/([^/]+)$/)
    if (detailMatch) {
      const id = decodeURIComponent(detailMatch[1])
      const status = url.searchParams.get('status')
      const root = rootFor(id, status)
      const selected = root.interactions.find((item) => item.id === id)!
      const detail: InteractionDetail = {
        interaction: selected,
        root,
        runs: [runFor(selected)],
        snapshot_sequence: snapshotSequence,
      }
      await route.fulfill({ json: { data: detail } })
      return
    }

    if (path === '/observations/interactions') {
      forestRequests.push(url)
      const anchor = Number(url.searchParams.get('anchor_at'))
      const windowIndex = Number(url.searchParams.get('window_index') ?? 0)
      const status = url.searchParams.get('status')
      const cursor = url.searchParams.get('cursor')
      if (cursor) await remainingRootsReady
      const pageRoots = status
        ? [rootFor(atlas.id, status)]
        : windowIndex > 0
          ? [rootFor(atlas.id)]
          : cursor === 'remaining-roots'
            ? roots.slice(1).map((root) => rootFor(root.interactions[0].id))
            : [rootFor(atlas.id)]
      await route.fulfill({
        json: {
          data: {
            anchor_at: anchor,
            window_index: windowIndex,
            window_start: anchor - (windowIndex + 1) * DAY,
            window_end: anchor - windowIndex * DAY,
            roots: pageRoots,
            root_total: status || windowIndex > 0 ? 1 : 3,
            next_cursor: !status && windowIndex === 0 && !cursor ? 'remaining-roots' : null,
            snapshot_sequence: snapshotSequence,
          },
        },
      })
      return
    }

    await route.fallback()
  })

  return {
    releaseRemainingRoots,
    emit(event) {
      snapshotSequence = Math.max(snapshotSequence, event.sequence)
      const known = roots.flatMap((root) => root.interactions).find((item) => item.id === event.interaction_id)
      if (known) {
        known.last_event_sequence = event.sequence
        known.last_active_at = event.occurred_at
        known.visible_tail = `Updated at sequence ${event.sequence}`
      }
      streamEvents.push(event)
    },
    forestRequests,
    debugWrites,
  }
}

function node(page: Page, name: string, status: string): Locator {
  return page.getByRole('button', { name: `${name}, ${statusLabels[status] ?? status}`, exact: true })
}

async function viewportTransform(page: Page): Promise<{ x: number; y: number; zoom: number }> {
  return page.locator('.svelte-flow__viewport').evaluate((element) => {
    const matrix = new DOMMatrixReadOnly(getComputedStyle(element).transform)
    return { x: matrix.e, y: matrix.f, zoom: matrix.a }
  })
}

test.describe('Interaction Observation canvas', () => {
  test.beforeEach(async ({ page }) => {
    await prepareApp(page)
  })

  test('downloads the bundle as a file without entering the application router', async ({ page }) => {
    await installObservationFixture(page)
    await page.route('**/api/v1/observations/interactions/interaction-atlas/debug-bundle-tickets', (route) =>
      route.fulfill({
        json: {
          data: {
            download_url: '/api/v1/observations/debug-bundles/browser-export-fixture',
            expires_at: Date.now() + 60_000,
            through_sequence: 10,
          },
        },
      }),
    )
    const emptyZip = Buffer.alloc(22)
    emptyZip.writeUInt32LE(0x06054b50)
    await page.route('**/api/v1/observations/debug-bundles/browser-export-fixture', (route) =>
      route.fulfill({
        contentType: 'application/zip',
        headers: { 'content-disposition': 'attachment; filename="observation.zip"', 'cache-control': 'no-store' },
        body: emptyZip,
      }),
    )
    await page.goto('/logs')
    await node(page, 'Atlas', 'completed').click()
    const download = page.waitForEvent('download')
    await page.getByRole('button', { name: 'Debug bundle', exact: true }).click()
    const completed = await download
    expect(completed.suggestedFilename()).toBe('observation.zip')
    expect(await completed.failure()).toBeNull()
    await expect(page).toHaveURL('/logs')
  })

  test('lays out complete root trees and keeps pointer, keyboard, inspector, and overview controls operable', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    const fixture = await installObservationFixture(page, true)
    await page.goto('/logs')

    await expect(page.getByText('1 / 3 roots', { exact: true })).toBeVisible()
    await expect.poll(() => fixture.forestRequests.some((url) => url.searchParams.has('cursor'))).toBe(true)
    await page.getByRole('button', { name: 'Load and fit all roots' }).click()
    await expect(page.getByRole('progressbar', { name: 'Loading all roots before fitting' })).toBeVisible()
    fixture.releaseRemainingRoots()
    await expect(page.getByText('3 / 3 roots', { exact: true })).toBeVisible()
    await expect(node(page, 'Atlas', 'completed')).toHaveCount(1)

    await expect(async () => {
      const [atlasBox, borealBox, cinderBox, deltaBox] = await page.evaluate(
        (labels) =>
          labels.map((label) => {
            const element = document.querySelector(`.svelte-flow__node[aria-label="${label}"]`)
            return element?.getBoundingClientRect().toJSON() as DOMRect | undefined
          }),
        ['Atlas, Completed', 'Boreal, Waiting for client', 'Cinder, Running', 'Delta, Completed'],
      )
      expect(atlasBox).toBeDefined()
      expect(borealBox).toBeDefined()
      expect(cinderBox).toBeDefined()
      expect(deltaBox).toBeDefined()
      expect(atlasBox!.y).toBeLessThan(borealBox!.y)
      expect(Math.abs(borealBox!.y - cinderBox!.y)).toBeLessThan(8)
      expect(Math.abs(borealBox!.x - cinderBox!.x)).toBeGreaterThan(35)
      expect(deltaBox!.x).toBeGreaterThan(Math.max(borealBox!.right, cinderBox!.right))
    }).toPass({ timeout: 5_000 })

    await node(page, 'Atlas', 'completed').click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByRole('heading', { name: 'Atlas' })).toBeVisible()
    const widthBefore = (await inspector.boundingBox())!.width
    const resize = page.getByRole('slider', { name: 'Resize details inspector' })
    await resize.press('ArrowRight')
    await resize.press('ArrowRight')
    await expect.poll(async () => (await inspector.boundingBox())!.width).toBeGreaterThan(widthBefore)
    await page.getByRole('button', { name: 'Close', exact: true }).click()

    await node(page, 'Boreal', 'waiting_client').focus()
    await node(page, 'Boreal', 'waiting_client').press('Enter')
    await expect(inspector.getByRole('heading', { name: 'Boreal' })).toBeVisible()
    await page.getByRole('button', { name: 'Close', exact: true }).click()

    await page.getByRole('button', { name: 'Show minimap' }).click()
    await expect(page.getByLabel('Interaction forest minimap')).toBeVisible()
    await page.getByRole('button', { name: 'Hide minimap' }).click()
    await expect(page.getByLabel('Interaction forest minimap')).toHaveCount(0)

    const pane = page.locator('.svelte-flow__pane')
    const paneBox = (await pane.boundingBox())!
    const pointer = { x: paneBox.x + paneBox.width * 0.58, y: paneBox.y + paneBox.height * 0.52 }
    const beforeZoom = await viewportTransform(page)
    const localPointer = { x: pointer.x - paneBox.x, y: pointer.y - paneBox.y }
    const worldUnderPointer = {
      x: (localPointer.x - beforeZoom.x) / beforeZoom.zoom,
      y: (localPointer.y - beforeZoom.y) / beforeZoom.zoom,
    }
    await page.mouse.move(pointer.x, pointer.y)
    await page.mouse.wheel(0, -260)
    await expect.poll(async () => (await viewportTransform(page)).zoom).toBeGreaterThan(beforeZoom.zoom)
    const afterZoom = await viewportTransform(page)
    expect(Math.abs(worldUnderPointer.x * afterZoom.zoom + afterZoom.x - localPointer.x)).toBeLessThan(6)
    expect(Math.abs(worldUnderPointer.y * afterZoom.zoom + afterZoom.y - localPointer.y)).toBeLessThan(6)

    await page.mouse.move(pointer.x, pointer.y)
    await page.mouse.down()
    await page.mouse.move(pointer.x + 90, pointer.y + 45, { steps: 5 })
    await page.mouse.up()
    await expect.poll(async () => (await viewportTransform(page)).x).not.toBe(afterZoom.x)
  })

  test('preserves causal roots through filters and anchored history while live activity pauses and resumes follow', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')

    await node(page, 'Atlas', 'completed').click()
    await page.getByRole('button', { name: 'Close', exact: true }).click()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 300_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_projection_delta',
      payload: { text: 'new visible activity' },
    })
    const follow = page.getByRole('button', { name: 'New activity · Follow' })
    await expect(follow).toBeVisible({ timeout: 5_000 })
    await follow.click()
    await expect(follow).toHaveCount(0)

    await page.getByRole('button', { name: 'Filters' }).click()
    await page.getByLabel('Status', { exact: true }).click()
    await page.getByRole('option', { name: 'Completed', exact: true }).click()
    await page.getByRole('button', { name: 'Apply filters' }).click()
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()
    await expect(node(page, 'Boreal', 'waiting_client')).toBeVisible()
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    await expect(node(page, 'Atlas', 'completed').locator('article')).toHaveCSS('opacity', '1')
    await expect(node(page, 'Boreal', 'waiting_client').locator('article')).toHaveCSS('opacity', '0.42')
    await expect(node(page, 'Cinder', 'running').locator('article')).toHaveCSS('opacity', '0.42')

    const firstAnchor = fixture.forestRequests[0].searchParams.get('anchor_at')
    await page.getByRole('button', { name: 'Older' }).click()
    await expect.poll(() => fixture.forestRequests.at(-1)?.searchParams.get('window_index')).toBe('1')
    expect(fixture.forestRequests.at(-1)?.searchParams.get('anchor_at')).toBe(firstAnchor)

    await node(page, 'Atlas', 'completed').click()
    const historicalRequest = fixture.forestRequests.at(-1)!
    fixture.emit({
      sequence: 12,
      occurred_at: Number(historicalRequest.searchParams.get('anchor_at')),
      interaction_id: 'interaction-atlas',
      run_id: 'run-interaction-atlas',
      rejection_id: null,
      kind: 'interaction_updated',
      payload: { status: 'completed' },
    })
    const migration = page.getByRole('button', { name: 'This chain moved to a newer time page · Open latest' })
    await expect(migration).toBeVisible({ timeout: 5_000 })
    await migration.click()
    await expect.poll(() => fixture.forestRequests.at(-1)?.searchParams.get('window_index')).toBe('0')
    await expect(migration).toHaveCount(0)
  })

  test.describe('touch input', () => {
    test.use({ hasTouch: true })

    test('uses a full-screen touch inspector, confirms every Debug enable, and honors reduced motion', async ({
      page,
    }) => {
      await page.setViewportSize({ width: 390, height: 740 })
      await page.emulateMedia({ reducedMotion: 'reduce' })
      const fixture = await installObservationFixture(page)
      await page.goto('/logs')

      const runningDot = node(page, 'Cinder', 'running').locator('[data-status="running"] .status-dot')
      const waitingDot = node(page, 'Boreal', 'waiting_client').locator('[data-status="waiting_client"] .status-dot')
      await expect(runningDot).toHaveCSS('animation-name', 'none')
      await expect(waitingDot).toHaveCSS('animation-name', 'none')

      await node(page, 'Cinder', 'running').click()
      const inspector = page.getByRole('complementary', { name: 'Observation details' })
      const inspectorBox = (await inspector.boundingBox())!
      expect(inspectorBox.x).toBeLessThanOrEqual(1)
      expect(inspectorBox.width).toBeGreaterThanOrEqual(389)
      await expect(page.getByRole('slider', { name: 'Resize details inspector' })).toBeHidden()
      await page.getByRole('button', { name: 'Close', exact: true }).click()

      const debugSwitch = page.getByRole('switch', { name: 'Debug' })
      await debugSwitch.click()
      const confirmation = page.getByRole('alertdialog', { name: 'Enable Debug' })
      await expect(confirmation).toContainText('Confirm every enable action.')
      await confirmation.getByRole('button', { name: 'Cancel' }).click()
      expect(fixture.debugWrites).toHaveLength(0)

      await debugSwitch.click()
      await page
        .getByRole('alertdialog', { name: 'Enable Debug' })
        .getByRole('button', { name: 'Enable Debug' })
        .click()
      await expect(debugSwitch).toBeChecked()
      expect(fixture.debugWrites).toEqual([{ enabled: true, confirmed: true }])
      await debugSwitch.click()
      await expect(debugSwitch).not.toBeChecked()
      expect(fixture.debugWrites.at(-1)).toEqual({ enabled: false, confirmed: false })
      await debugSwitch.click()
      await expect(page.getByRole('alertdialog', { name: 'Enable Debug' })).toBeVisible()

      await page.getByRole('alertdialog', { name: 'Enable Debug' }).getByRole('button', { name: 'Cancel' }).click()
      const paneBox = (await page.locator('.svelte-flow__pane').boundingBox())!
      const centerX = paneBox.x + paneBox.width / 2
      const centerY = paneBox.y + paneBox.height / 2
      const beforePinch = await viewportTransform(page)
      const session = await page.context().newCDPSession(page)
      await session.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 2 })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchStart',
        touchPoints: [
          { x: centerX - 25, y: centerY, id: 0 },
          { x: centerX + 25, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchMove',
        touchPoints: [
          { x: centerX - 50, y: centerY, id: 0 },
          { x: centerX + 50, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchMove',
        touchPoints: [
          { x: centerX - 75, y: centerY, id: 0 },
          { x: centerX + 75, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] })
      await expect.poll(async () => (await viewportTransform(page)).zoom).not.toBe(beforePinch.zoom)
      await session.detach()
    })
  })
})
