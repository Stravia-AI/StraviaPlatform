import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

interface AllowanceFixture {
  provider_id: string
  [key: string]: unknown
}

const freshSnapshot = {
  guard_supported: true,
  missing_guarded_keys: [],
  suspension: null,
  provider_id: 'provider-alpha',
  provider_name: 'Alpha account',
  catalog_provider_id: 'openai-codex',
  channel: 'codex',
  plan_label: 'Pro',
  account_label: 'alpha@example.test',
  status: 'fresh',
  fetched_at: '2026-09-01T12:00:00Z',
  allowances: [
    {
      key: 'weekly',
      guarded: false,
      label: 'Weekly',
      kind: 'quota_window',
      used: { value: 55.625, unit: 'tokens' },
      remaining: { value: 44.375, unit: 'tokens' },
      limit: { value: 100, unit: 'tokens' },
      used_percent: 55.625,
      reset_at: 1788883200000,
      condition: 'normal',
      forecast: { status: 'no_risk', projected_remaining_percent: 24.5 },
    },
    {
      key: 'credits_balance',
      guarded: false,
      label: 'Credit balance',
      kind: 'balance',
      remaining: { value: 0, unit: 'currency', currency: 'USD' },
      condition: 'exhausted',
      forecast: { status: 'unknown' },
    },
    {
      key: 'credits_balance_cny',
      guarded: false,
      label: 'Credit balance',
      kind: 'balance',
      remaining: { value: 9.99, unit: 'currency', currency: 'CNY' },
      condition: 'normal',
      forecast: { status: 'unknown' },
    },
  ],
  models: [
    {
      model: 'gpt-5.3-codex-spark',
      allowances: [
        {
          key: 'weekly_model',
          guarded: false,
          label: 'Weekly',
          kind: 'quota_window',
          remaining: { value: 25, unit: 'tokens' },
          reset_at: 1788282000000,
          condition: 'tight',
          forecast: { status: 'will_exhaust', exhausts_at: 1788200000000 },
        },
      ],
    },
  ],
}

const staleSnapshot = {
  guard_supported: true,
  missing_guarded_keys: [],
  suspension: null,
  provider_id: 'provider-beta',
  provider_name: 'Beta account',
  catalog_provider_id: 'openai',
  channel: 'codex',
  status: 'stale',
  fetched_at: '2026-09-01T11:30:00Z',
  allowances: [
    {
      key: 'weekly',
      guarded: false,
      label: 'Weekly',
      kind: 'quota_window',
      used_percent: 111.25,
      used: { value: 111.25, unit: 'requests' },
      limit: { value: 100, unit: 'requests' },
      reset_at: 1788796800000,
      condition: 'exhausted',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 0, exhausts_at: 1788700000000 },
    },
  ],
  models: [],
  error: { category: 'rate_limited', message: 'safe backend message' },
}

const errorSnapshot = {
  guard_supported: false,
  missing_guarded_keys: [],
  suspension: null,
  provider_id: 'provider-gamma',
  provider_name: 'Gamma account',
  catalog_provider_id: 'github-copilot',
  channel: 'default',
  status: 'error',
  allowances: [],
  models: [],
  error: { category: 'authentication', message: 'safe backend message' },
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

async function mockAllowances(page: Page, snapshots: AllowanceFixture[]) {
  const posts: string[] = []
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    if (request.method() === 'POST') posts.push(path)
    const providerId = path.match(/provider-allowances\/([^/]+?)(?:\/refresh)?$/)?.[1]
    if (providerId) {
      await route.fulfill({ json: { data: snapshots.find((snapshot) => snapshot.provider_id === providerId) ?? null } })
      return
    }
    await route.fulfill({
      json: {
        data: snapshots.map((snapshot) => ({
          provider_id: snapshot.provider_id,
          guard_supported: snapshot.guard_supported,
          suspension: snapshot.suspension,
          provider_name: snapshot.provider_name,
          catalog_provider_id: snapshot.catalog_provider_id,
          channel: snapshot.channel,
          snapshot,
          refreshing: false,
        })),
      },
    })
  })
  return posts
}

async function selectFilter(page: Page, label: string, option: string): Promise<void> {
  await page.getByRole('button', { name: label, exact: true }).click()
  await page.getByRole('option', { name: option, exact: true }).click()
}

function requestGate() {
  const { promise, resolve } = Promise.withResolvers<void>()
  return { promise, release: resolve }
}

test('keeps confirmed guards and open details when an older account read finishes after saving', async ({ page }) => {
  await page.clock.install()
  const snapshots = structuredClone([freshSnapshot])
  await mockAllowances(page, snapshots)
  const readStarted = requestGate()
  const releaseRead = requestGate()
  const readFinished = requestGate()
  try {
    await page.goto('/allowances')
    const provider = page.getByTestId('allowance-provider-provider-alpha')
    const toggleDetails = provider.getByRole('button', { name: 'Alpha account allowance details' })
    await toggleDetails.click()
    const details = provider.getByRole('table', { name: 'Alpha account allowance details' })
    const weekly = details.getByRole('switch', { name: 'Pause this provider when Weekly window is exhausted' })
    await expect(weekly).not.toBeChecked()
    const oldSnapshot = structuredClone(snapshots[0])
    let heldReads = 0
    await page.route('**/api/v1/provider-allowances/provider-alpha', async (route) => {
      const response = ++heldReads === 1 ? oldSnapshot : structuredClone(snapshots[0])
      readStarted.release()
      await releaseRead.promise
      try {
        await route.fulfill({ json: { data: response } })
      } finally {
        readFinished.release()
      }
    })
    await page.route('**/api/v1/provider-allowances/provider-alpha/guards', async (route) => {
      const { keys } = route.request().postDataJSON() as { keys: string[] }
      snapshots[0] = {
        ...snapshots[0],
        allowances: snapshots[0].allowances.map((item) => ({ ...item, guarded: keys.includes(item.key) })),
      }
      await route.fulfill({
        json: { data: { keys, target: { ...snapshots[0], snapshot: snapshots[0], refreshing: true } } },
      })
    })
    await page.clock.fastForward(180_001)
    await readStarted.promise
    await weekly.click()
    await expect(weekly).toBeChecked()
    await expect(weekly).toBeEnabled()

    releaseRead.release()
    await readFinished.promise
    await expect(details).toBeVisible()
    await expect(weekly).toBeChecked()
    await expect(details.getByText('Weekly window', { exact: true })).toBeVisible()
  } finally {
    releaseRead.release()
  }
})

test('publishes successful bulk refreshes and toasts the first target failure even when it finishes last', async ({
  page,
}) => {
  await mockAllowances(page, [freshSnapshot, staleSnapshot, errorSnapshot])
  const releaseFirstFailure = requestGate()
  const releaseSuccess = requestGate()
  const successStarted = requestGate()
  const laterFailureFinished = requestGate()
  await page.route('**/api/v1/provider-allowances/*/refresh', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path.includes('provider-alpha')) {
      await releaseFirstFailure.promise
      await route.fulfill({ status: 400, json: { error: 'Alpha refresh rejected' } })
    } else if (path.includes('provider-beta')) {
      successStarted.release()
      await releaseSuccess.promise
      await route.fulfill({
        json: {
          data: {
            ...staleSnapshot,
            status: 'fresh',
            error: undefined,
            allowances: [
              {
                ...staleSnapshot.allowances[0],
                used_percent: 25,
                used: { value: 25, unit: 'requests' },
                condition: 'normal',
              },
            ],
          },
        },
      })
    } else {
      await route.fulfill({ status: 400, json: { error: 'Gamma refresh rejected' } })
      laterFailureFinished.release()
    }
  })
  try {
    await page.goto('/allowances')
    const beta = page.getByTestId('allowance-provider-provider-beta')
    await beta.getByRole('button', { name: 'Beta account allowance details' }).click()
    const details = beta.getByRole('table', { name: 'Beta account allowance details' })
    await expect(details.getByRole('progressbar', { name: 'Weekly window Remaining' })).toHaveAttribute(
      'aria-valuenow',
      '0',
    )
    await page.getByRole('button', { name: 'Refresh all' }).click()
    await Promise.all([successStarted.promise, laterFailureFinished.promise])
    releaseSuccess.release()
    await expect(details.getByRole('progressbar', { name: 'Weekly window Remaining' })).toHaveAttribute(
      'aria-valuenow',
      '75',
    )
    await expect(page.getByRole('button', { name: 'Refresh all' })).toBeDisabled()
    releaseFirstFailure.release()
    await expect(page.getByText(/Alpha refresh rejected/)).toBeVisible()
    await expect(page.getByText(/Gamma refresh rejected/)).toHaveCount(0)
    await expect(details).toBeVisible()
    await expect(details.getByRole('progressbar', { name: 'Weekly window Remaining' })).toHaveAttribute(
      'aria-valuenow',
      '75',
    )
    await expect(page.getByRole('button', { name: 'Refresh all' })).toBeEnabled()
  } finally {
    releaseFirstFailure.release()
    releaseSuccess.release()
  }
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`guard confirmation remains editable while upstream and metadata wait (${locale})`, async ({ page }) => {
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    const upstream = requestGate()
    const metadata = requestGate()
    const saveGate = requestGate()
    const saveStarted = requestGate()
    let saves = 0
    let lists = 0
    let snapshot = structuredClone(freshSnapshot)
    const suspended = {
      suspended_at: '2026-09-01T12:00:00Z',
      triggered_keys: ['credits_balance'],
      earliest_reset_at: null,
    }
    await page.route('**/api/v1/provider-allowances**', async (route) => {
      const path = new URL(route.request().url()).pathname
      if (route.request().method() === 'PUT') {
        const { keys } = route.request().postDataJSON() as { keys: string[] }
        saves += 1
        saveStarted.release()
        if (saves === 1) await saveGate.promise
        snapshot = {
          ...snapshot,
          allowances: snapshot.allowances.map((item) => ({ ...item, guarded: keys.includes(item.key) })),
        }
        await route.fulfill({ json: { data: { keys, target: { ...snapshot, snapshot, refreshing: true } } } })
        return
      }
      if (path.endsWith('/provider-allowances')) {
        if (++lists > 1) await metadata.promise
        await route.fulfill({ json: { data: [{ ...snapshot, snapshot, refreshing: false }] } })
        return
      }
      const captured = structuredClone(snapshot)
      if (saves > 0) await upstream.promise
      await route.fulfill({
        json: {
          data:
            saves > 1
              ? {
                  ...captured,
                  suspension: captured.allowances.find((item) => item.key === 'credits_balance')?.guarded
                    ? suspended
                    : null,
                  allowances: captured.allowances.map((item) =>
                    item.key === 'weekly'
                      ? { ...item, remaining: { value: 25, unit: 'tokens' }, used_percent: 75 }
                      : item,
                  ),
                }
              : captured,
        },
      })
    })
    try {
      await page.goto('/allowances')
      const provider = page.getByTestId('allowance-provider-provider-alpha')
      await provider
        .getByRole('button', {
          name: locale === 'en-US' ? 'Alpha account allowance details' : 'Alpha account 额度详情',
        })
        .click()
      const weekly = provider.getByRole('switch', {
        name: locale === 'en-US' ? 'Pause this provider when Weekly window is exhausted' : '每周窗口耗尽时暂停此供应商',
      })
      await weekly.click()
      await saveStarted.promise
      await expect(weekly).toHaveAttribute('aria-busy', 'true')
      await expect(weekly).not.toBeChecked()
      await expect(weekly).toBeDisabled()
      saveGate.release()
      await expect(weekly).toBeChecked()
      await expect(weekly).toBeEnabled()
      await expect(weekly).toHaveAttribute('aria-busy', 'false')
      const credits = provider
        .getByRole('switch', {
          name:
            locale === 'en-US' ? 'Pause this provider when Account balance is exhausted' : '账户余额耗尽时暂停此供应商',
          exact: true,
        })
        .first()
      await credits.click()
      await expect(credits).toBeChecked()
      await expect(credits).toBeEnabled()
      await expect(provider.getByTestId('allowance-suspension')).toHaveCount(0)
      upstream.release()
      await expect(provider.getByTestId('allowance-suspension')).toBeVisible()
      await expect(weekly).toBeChecked()
      await expect(credits).toBeChecked()
      await expect(weekly).toBeEnabled()
      await expect(
        provider
          .getByRole('table')
          .getByRole('progressbar', { name: locale === 'en-US' ? 'Weekly window Remaining' : '每周窗口 剩余' }),
      ).toHaveAttribute('aria-valuenow', '25')
      metadata.release()
    } finally {
      saveGate.release()
      upstream.release()
      metadata.release()
    }
  })

  test(`guards preserve missing keys and confirmed values (${locale})`, async ({ page }) => {
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    const submissions: string[][] = []
    let fail = false
    let snapshot = {
      ...freshSnapshot,
      missing_guarded_keys: ['old-window'],
      suspension: {
        suspended_at: '2026-09-01T12:00:00Z',
        triggered_keys: ['credits_balance'],
        earliest_reset_at: 1788883200000,
      },
    }
    await page.route('**/api/v1/provider-allowances**', async (route) => {
      const path = new URL(route.request().url()).pathname
      if (route.request().method() === 'PUT') {
        const { keys } = route.request().postDataJSON() as { keys: string[] }
        submissions.push(keys)
        if (fail) {
          await route.fulfill({ status: 400, json: { error: 'Guard update rejected' } })
          return
        }
        snapshot = {
          ...snapshot,
          missing_guarded_keys: keys.includes('old-window') ? ['old-window'] : [],
          allowances: snapshot.allowances.map((item) => ({ ...item, guarded: keys.includes(item.key) })),
        }
        await route.fulfill({ json: { data: { keys, target: { ...snapshot, snapshot, refreshing: true } } } })
        return
      }
      await route.fulfill({
        json: {
          data: path.endsWith('/provider-allowances')
            ? [
                { ...snapshot, snapshot, refreshing: false },
                { ...errorSnapshot, snapshot: errorSnapshot, refreshing: false },
              ]
            : path.includes('provider-gamma')
              ? errorSnapshot
              : snapshot,
        },
      })
    })
    await page.goto('/allowances')
    const weekly = page.getByRole('switch', {
      name: locale === 'en-US' ? 'Pause this provider when Weekly window is exhausted' : '每周窗口耗尽时暂停此供应商',
    })
    // 暂停状态属于必须立即可见的异常，不能藏在默认折叠的详情里
    await expect(page.getByTestId('allowance-suspension').first()).toBeVisible()
    const pause = page
      .getByRole('button', { name: locale === 'en-US' ? 'Allowance paused' : '额度暂停', exact: true })
      .first()
    await pause.click()
    await expect(page.getByRole('tooltip')).toContainText('credits_balance')
    await expect(weekly).toHaveCount(0)
    await pause.press('Escape')
    await expect(page.getByRole('tooltip')).not.toBeVisible()
    await expect(weekly).toHaveCount(0)
    await page
      .getByRole('button', { name: locale === 'en-US' ? 'Alpha account allowance details' : 'Alpha account 额度详情' })
      .click()
    const details = page.getByRole('table', {
      name: locale === 'en-US' ? 'Alpha account allowance details' : 'Alpha account 额度详情',
    })
    await expect(
      details.getByText(locale === 'en-US' ? 'Pause trigger' : '暂停触发条目', { exact: true }),
    ).toBeVisible()
    await page
      .getByRole('button', {
        name: locale === 'en-US' ? 'Show model allowances for Alpha account' : '展开 Alpha account 的模型额度',
      })
      .first()
      .click()
    const model = page.getByRole('button', { name: 'gpt-5.3-codex-spark' }).first()
    await model.click()
    await expect(model.locator('..').getByRole('switch')).toHaveCount(0)
    await expect(page.getByTestId('allowance-provider-provider-gamma').getByRole('switch')).toHaveCount(0)
    await weekly.click()
    await expect.poll(() => submissions[0]).toEqual(['old-window', 'weekly'])
    await expect(weekly).toBeChecked()
    fail = true
    await weekly.click()
    await expect(page.getByText('Guard update rejected').first()).toBeVisible()
    await expect(weekly).toBeChecked()
    fail = false
    await page
      .getByRole('button', { name: locale === 'en-US' ? 'Stop pausing on old-window' : '不再因 old-window 暂停' })
      .first()
      .click()
    await expect.poll(() => submissions.at(-1)).toEqual(['weekly'])
    await expect(page.getByRole('button', { name: /old-window/ })).toHaveCount(0)
  })
}

test('pauses invalid credentials without blocking healthy providers and resumes after repair', async ({ page }) => {
  await page.clock.install()
  let credentialStatus = 'invalid'
  let providerReads = 0
  let targetReads = 0
  const requests: string[] = []
  await page.route('**/api/v1/providers', async (route) => {
    providerReads++
    await route.fulfill({
      json: {
        data: [
          { id: freshSnapshot.provider_id, credential_status: credentialStatus },
          { id: staleSnapshot.provider_id, credential_status: 'ok' },
        ],
      },
    })
  })
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    const providerId = path.match(/provider-allowances\/([^/]+?)(?:\/refresh)?$/)?.[1]
    if (providerId) {
      requests.push(`${request.method()} ${providerId}`)
      await route.fulfill({ json: { data: providerId === freshSnapshot.provider_id ? freshSnapshot : staleSnapshot } })
      return
    }
    targetReads++
    await route.fulfill({
      json: {
        data: [freshSnapshot, staleSnapshot].map((snapshot) => ({
          provider_id: snapshot.provider_id,
          provider_name: snapshot.provider_name,
          catalog_provider_id: snapshot.catalog_provider_id,
          channel: snapshot.channel,
          snapshot: snapshot.provider_id === freshSnapshot.provider_id ? undefined : snapshot,
          refreshing: false,
        })),
      },
    })
  })
  await page.goto('/allowances')
  const invalid = page.getByTestId(`allowance-provider-${freshSnapshot.provider_id}`)
  const healthy = page.getByTestId(`allowance-provider-${staleSnapshot.provider_id}`)
  const invalidRefresh = invalid.getByRole('button', { name: 'Refresh Alpha account' })
  await expect(invalidRefresh).toBeDisabled()
  await expect(invalid.getByTestId(`allowance-credential-invalid-${freshSnapshot.provider_id}`)).toBeVisible()
  await expect(invalid.getByRole('link')).toHaveAttribute(
    'href',
    `/providers/${freshSnapshot.provider_id}?view=connection`,
  )
  await expect(page.getByTestId(`allowance-loading-${freshSnapshot.provider_id}`)).toHaveCount(0)
  await healthy.getByRole('button', { name: 'Refresh Beta account' }).click()
  await expect.poll(() => requests).toContain(`POST ${staleSnapshot.provider_id}`)
  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => requests.filter((request) => request === `POST ${staleSnapshot.provider_id}`).length).toBe(2)
  const readsBeforePoll = { providers: providerReads, targets: targetReads }
  await page.clock.fastForward(180_001)
  await expect.poll(() => providerReads).toBeGreaterThan(readsBeforePoll.providers)
  await expect.poll(() => targetReads).toBeGreaterThan(readsBeforePoll.targets)
  expect(requests.filter((request) => request.endsWith(freshSnapshot.provider_id))).toEqual([])

  credentialStatus = 'ok'
  await page.clock.fastForward(180_001)
  await expect(invalidRefresh).toBeEnabled()
  await expect.poll(() => requests).toContain(`GET ${freshSnapshot.provider_id}`)
  await invalidRefresh.click()
  await expect.poll(() => requests).toContain(`POST ${freshSnapshot.provider_id}`)
  await expect(invalid.getByTestId(`allowance-credential-invalid-${freshSnapshot.provider_id}`)).toHaveCount(0)
})

test('pauses immediately when a snapshot reports persisted credential invalidation', async ({ page }) => {
  await page.clock.install()
  let credentialStatus = 'ok'
  const invalidSnapshot = {
    ...freshSnapshot,
    status: 'stale',
    error: {
      category: 'authentication',
      message: 'Credential invalid; allowance fetching is paused until the credential is updated.',
    },
  }
  const snapshots: AllowanceFixture[] = [freshSnapshot, staleSnapshot]
  const posts = await mockAllowances(page, snapshots)
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          { id: freshSnapshot.provider_id, credential_status: credentialStatus },
          { id: staleSnapshot.provider_id, credential_status: 'ok' },
        ],
      },
    })
  })
  await page.goto('/allowances')
  const provider = page.getByTestId(`allowance-provider-${freshSnapshot.provider_id}`)
  const refresh = provider.getByRole('button', { name: 'Refresh Alpha account' })
  await expect(refresh).toBeEnabled()
  snapshots[0] = invalidSnapshot
  credentialStatus = 'invalid'
  await refresh.click()
  await expect(refresh).toBeDisabled()
  await expect(provider.getByTestId(`allowance-credential-invalid-${freshSnapshot.provider_id}`)).toBeVisible()
  await expect(provider.getByText(/44[.,]3/)).toBeVisible()
  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts.filter((path) => path.includes(staleSnapshot.provider_id)).length).toBe(1)
  expect(posts.filter((path) => path.includes(freshSnapshot.provider_id))).toHaveLength(1)
  snapshots[0] = freshSnapshot
  credentialStatus = 'ok'
  await page.clock.fastForward(180_001)
  await expect(refresh).toBeEnabled()
  await refresh.click()
  await expect.poll(() => posts.filter((path) => path.includes(freshSnapshot.provider_id)).length).toBe(2)
})

test('renders the matrix, shared summary, timeline, forecast, model details, and refresh actions', async ({ page }) => {
  await page.clock.setFixedTime(new Date('2026-09-02T12:00:00Z'))
  const posts = await mockAllowances(page, [errorSnapshot, staleSnapshot, freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: 'Allowance overview' })).toBeVisible()
  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  await expect(matrix).toBeVisible()
  await expect(matrix.getByText('Alpha account')).toBeVisible()
  await expect(matrix.getByText('Beta account')).toBeVisible()
  await expect(matrix.getByText('Gamma account')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('Stale', { exact: true })).toBeVisible()
  await expect(matrix.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(matrix.getByText('Exhausted', { exact: true }).first()).toBeVisible()
  await expect(matrix.getByText('0 USD')).toBeVisible()
  // 币种后缀 key（credits_balance_cny）与普通 credits_balance 共用同一标签展示
  await expect(matrix.getByText('9.99 CNY')).toBeVisible()
  await expect(matrix.getByText('Account balance', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('Showing the last successful result because this refresh failed.')).toBeVisible()
  await expect(matrix.getByText('Reconnect this provider or update its credential.')).toBeVisible()

  const conditionSummary = page.getByRole('region', { name: 'Allowance summary' })
  const forecastPanel = page.getByRole('region', { name: 'Exhaustion forecast' })
  await expect(conditionSummary.getByText('Exhausted', { exact: true })).toBeVisible()
  await expect(conditionSummary.getByText(/Lowest usable remaining/)).toBeVisible()
  // 汇总只统计并定位例外；耗尽项明细按需展开
  const emptyWindows = page.getByTestId('allowance-empty-windows')
  await expect(emptyWindows).toHaveCount(0)
  await conditionSummary.getByRole('button', { name: 'Exhausted items (1)' }).click()
  await expect(emptyWindows).toContainText('Beta account')
  await expect(emptyWindows).toContainText('Weekly window')
  await expect(emptyWindows).not.toContainText('Alpha account')
  await expect(page.getByRole('heading', { name: 'Reset timeline' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Exhaustion forecast' })).toBeVisible()
  // 预报依据收进问号，不再常驻
  await expect(page.getByText('Based on the current window')).toHaveCount(0)
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')
  await forecastPanel.getByRole('button', { name: 'Exception details (1)' }).click()
  await expect(forecastPanel).toContainText('exhausted at')
  await expect(forecastPanel).not.toContainText('may exhaust')

  const timeline = page.getByRole('list', { name: 'Reset timeline' })
  const alphaReset = timeline.getByRole('button', { name: /Locate Alpha account · Weekly window/ })
  const betaReset = timeline.getByRole('button', { name: /Locate Beta account · Weekly window/ })
  const alphaId = await alphaReset.getAttribute('aria-controls')
  const betaId = await betaReset.getAttribute('aria-controls')
  const alphaAllowance = matrix.locator(`[id=${JSON.stringify(alphaId)}]`)
  const normalBackground = await alphaAllowance.evaluate((element) => getComputedStyle(element).backgroundColor)
  await alphaReset.hover()
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveAttribute('id', alphaId!)
  await expect
    .poll(() => alphaAllowance.evaluate((element) => getComputedStyle(element).backgroundColor))
    .not.toBe(normalBackground)
  await betaReset.hover()
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveAttribute('id', betaId!)
  await page.mouse.move(0, 0)
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveCount(0)
  await expect(alphaAllowance).toHaveCSS('background-color', normalBackground)
  await alphaReset.focus()
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveAttribute('id', alphaId!)
  await betaReset.focus()
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveAttribute('id', betaId!)
  await page.getByLabel('Search providers').focus()
  await expect(matrix.locator('[data-highlighted="true"]')).toHaveCount(0)
  await alphaReset.click()
  await expect(alphaAllowance).toBeFocused()
  await expect(matrix.getByRole('button', { name: 'Alpha account allowance details' })).toHaveAttribute(
    'aria-expanded',
    'false',
  )

  await matrix.getByRole('button', { name: 'Alpha account allowance details' }).click()
  // 详情只展示上游账号标识；供应商目录 ID / 渠道不再展示，没有标识的供应商不出现该行
  await expect(page.getByTestId('allowance-account-provider-alpha')).toHaveText('alpha@example.test')
  await expect(matrix.getByText('Account', { exact: true })).toHaveCount(1)
  await expect(matrix.getByText('openai-codex / codex')).toHaveCount(0)
  await matrix.getByLabel('Show model allowances for Alpha account').click()
  await matrix.getByText('gpt-5.3-codex-spark').click()
  await expect(matrix.getByText(/Resets/).last()).toBeVisible()
  await matrix.getByRole('button', { name: 'Beta account allowance details' }).click()
  await expect(matrix.getByRole('button', { name: 'Beta account allowance details' })).toHaveAttribute(
    'aria-expanded',
    'true',
  )
  await expect(page.getByTestId('allowance-account-provider-beta')).toHaveCount(0)
  await expect(matrix.getByText('openai / codex')).toHaveCount(0)

  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-beta/refresh')
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-gamma/refresh')
  await matrix.getByRole('button', { name: 'Refresh Alpha account' }).click()
  await expect
    .poll(() => posts.filter((path) => path === '/api/v1/provider-allowances/provider-alpha/refresh').length)
    .toBe(2)
})

test('renders provider shells first and fills each group as its snapshot arrives', async ({ page }) => {
  let releaseSnapshot!: () => void
  const gate = new Promise<void>((resolve) => (releaseSnapshot = resolve))
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    const providerId = path.match(/provider-allowances\/([^/]+)$/)?.[1]
    if (providerId && providerId !== 'refresh' && request.method() === 'GET') {
      await gate
      await route.fulfill({ json: { data: freshSnapshot } })
      return
    }
    await route.fulfill({
      json: {
        data: [
          {
            provider_id: freshSnapshot.provider_id,
            provider_name: freshSnapshot.provider_name,
            catalog_provider_id: freshSnapshot.catalog_provider_id,
            channel: freshSnapshot.channel,
            refreshing: true,
          },
        ],
      },
    })
  })
  await page.goto('/allowances')

  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  const conditionSummary = page.getByRole('region', { name: 'Allowance summary' })
  // 组头先行渲染；快照到达前行区与聚合区各自转圈，不出全屏骨架
  await expect(matrix.getByText('Alpha account')).toBeVisible()
  await expect(matrix.getByTestId('allowance-loading-provider-alpha')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(0)
  await expect(conditionSummary.getByText('Normal', { exact: true })).toHaveCount(0)

  releaseSnapshot()
  await expect(matrix.getByText('Weekly window', { exact: true })).toBeVisible()
  await expect(matrix.getByTestId('allowance-loading-provider-alpha')).toHaveCount(0)
  await expect(conditionSummary.getByText('Normal', { exact: true })).toBeVisible()
})

test('keeps multiple model allowances open and distinguishes unknown utilization from zero', async ({ page }) => {
  await mockAllowances(page, [
    {
      ...freshSnapshot,
      models: [
        freshSnapshot.models[0],
        { model: 'gpt-5.4', allowances: [{ ...freshSnapshot.models[0].allowances[0], label: 'GPT-5.4 window' }] },
      ],
    },
  ])
  await page.goto('/allowances')
  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  await matrix.getByRole('button', { name: 'Alpha account allowance details' }).click()
  await matrix.getByRole('button', { name: 'Show model allowances for Alpha account' }).click()
  const opus = matrix.getByRole('button', { name: 'gpt-5.3-codex-spark', exact: true })
  const sonnet = matrix.getByRole('button', { name: 'gpt-5.4', exact: true })
  await opus.click()
  await sonnet.click()
  await expect(opus).toHaveAttribute('aria-expanded', 'true')
  await expect(sonnet).toHaveAttribute('aria-expanded', 'true')
  await expect(matrix.getByText('GPT-5.4 window')).toBeVisible()
  await sonnet.click()
  await expect(opus).toHaveAttribute('aria-expanded', 'true')
  await expect(matrix.getByRole('progressbar', { name: 'Weekly window Remaining' })).toHaveAttribute(
    'aria-valuenow',
    '44.375',
  )
  // 余额没有上限可对照，不画进度条，避免把“未知比例”渲染成 0%
  await expect(matrix.getByRole('progressbar', { name: /Account balance/ })).toHaveCount(0)
})

test('switches every allowance value between remaining and used and remembers the choice', async ({ page }) => {
  await mockAllowances(page, [freshSnapshot, staleSnapshot])
  await page.goto('/allowances')
  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  const alpha = matrix.getByTestId('allowance-provider-provider-alpha')
  const beta = matrix.getByTestId('allowance-provider-provider-beta')
  const remaining = page.getByRole('tab', { name: 'Remaining' })
  const used = page.getByRole('tab', { name: 'Used' })
  await expect(remaining).toHaveAttribute('aria-selected', 'true')
  await expect(alpha).toContainText('44.38%')
  await expect(alpha).toContainText('9.99 CNY')

  await used.click()
  await expect(used).toHaveAttribute('aria-selected', 'true')
  await expect(alpha).toContainText('55.63%')
  await expect(alpha).not.toContainText('44.38%')
  await expect(beta).toContainText('111.25%')
  // 账户余额本身就是剩余金额，不随显示方式变化
  await expect(alpha).toContainText('9.99 CNY')
  await alpha.getByRole('button', { name: 'Alpha account allowance details' }).click()
  const details = matrix.getByRole('table', { name: 'Alpha account allowance details' })
  await expect(details.getByRole('columnheader', { name: 'Used', exact: true })).toBeVisible()
  await expect(details.getByRole('progressbar', { name: 'Weekly window Used' })).toHaveAttribute(
    'aria-valuenow',
    '55.625',
  )

  await used.click()
  await expect(used).toHaveAttribute('aria-selected', 'true')
  await page.reload()
  await expect(page.getByRole('tab', { name: 'Used' })).toHaveAttribute('aria-selected', 'true')
  await expect(matrix.getByTestId('allowance-provider-provider-alpha')).toContainText('55.63%')
})

test('keeps allowance details visible when refresh replaces provider snapshots', async ({ page }) => {
  const snapshots = structuredClone([freshSnapshot, staleSnapshot])
  const posts = await mockAllowances(page, snapshots)
  await page.goto('/allowances')
  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  const details = matrix.getByRole('table', { name: 'Alpha account allowance details' })
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('0 USD')).toBeVisible()
  await matrix.getByRole('button', { name: 'Alpha account allowance details' }).click()
  await expect(details).toBeVisible()

  snapshots[0].allowances[1].remaining!.value = 12
  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect(details.getByText('12 USD')).toBeVisible()
  await expect(details.getByText('Weekly window', { exact: true })).toBeVisible()

  snapshots[0].allowances[1].remaining!.value = 24
  await matrix.getByRole('button', { name: 'Refresh Alpha account' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect(details.getByText('24 USD')).toBeVisible()
  await expect(details.getByText('Weekly window', { exact: true })).toBeVisible()
})

test('does not treat an exhausted allowance without a reset date as exhausted', async ({ page }) => {
  await mockAllowances(page, [freshSnapshot])
  await page.goto('/allowances')

  const matrix = page.getByRole('list', { name: 'Allowance matrix' })
  const provider = matrix.getByTestId('allowance-provider-provider-alpha')
  const forecastPanel = page.getByRole('region', { name: 'Exhaustion forecast' })
  await expect(matrix.getByText('0 USD')).toBeVisible()
  await expect(provider.getByText('Exhausted', { exact: true })).toHaveCount(0)
  await expect(
    page.getByRole('region', { name: 'Allowance summary' }).getByText('Normal', { exact: true }),
  ).toBeVisible()
  await expect(page.getByTestId('allowance-empty-windows')).toHaveCount(0)
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')

  await selectFilter(page, 'All allowance states', 'Exhausted')
  await expect(page.getByText('No allowances match these filters.')).toBeVisible()
})

test('search and all filters drive the same visible collection', async ({ page }) => {
  await page.clock.setFixedTime(new Date('2026-09-02T12:00:00Z'))
  await mockAllowances(page, [errorSnapshot, staleSnapshot, freshSnapshot])
  await page.goto('/allowances')

  const search = page.getByLabel('Search providers')
  const timelinePanel = page.getByRole('region', { name: 'Reset timeline' })
  const forecastPanel = page.getByRole('region', { name: 'Exhaustion forecast' })
  await search.fill('alpha')
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Alpha account')).toBeVisible()
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Beta account')).toHaveCount(0)
  await expect(timelinePanel).not.toContainText('Beta account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')

  await search.fill('Weekly')
  await expect(page.getByText('No allowances match these filters.')).toBeVisible()
  await search.fill('')

  await selectFilter(page, 'All provider types', 'openai / codex')
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Beta account')).toBeVisible()
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(timelinePanel).toContainText('Beta account')
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel).toContainText('Current windows are already exhausted')

  await selectFilter(page, 'Provider type: openai / codex', 'All provider types')
  await selectFilter(page, 'All allowance states', 'Exhausted')
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Beta account')).toBeVisible()
  await expect(timelinePanel).toContainText('Beta account')
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )

  await selectFilter(page, 'Allowance state: Exhausted', 'All allowance states')
  await selectFilter(page, 'All data states', 'Unavailable')
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Gamma account')).toBeVisible()
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(timelinePanel).not.toContainText('Beta account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 0')
})

test('renders the empty and request-error states with recovery guidance', async ({ page }) => {
  await mockAllowances(page, [])
  await page.goto('/allowances')
  await expect(page.getByRole('heading', { name: 'No allowance data available' })).toBeVisible()
  await expect(page.getByText(/Enable and connect a supported provider/)).toBeVisible()
  await expect(page.getByRole('link', { name: 'Manage providers' })).toHaveAttribute('href', '/providers')
  // 没有支持对象时不显示无关的刷新入口与更新时间
  await expect(page.getByRole('button', { name: 'Refresh all' })).toHaveCount(0)
  await expect(page.getByText('Not updated yet')).toHaveCount(0)
  await expect(page.getByText(/^Updated /)).toHaveCount(0)

  await page.unroute('**/api/v1/provider-allowances**')
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    await route.fulfill({ status: 503, json: { error: 'allowance backend unavailable' } })
  })
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Provider allowances could not be loaded.' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Refresh all' })).toHaveCount(0)
  await page.unroute('**/api/v1/provider-allowances**')
  await mockAllowances(page, [freshSnapshot])
  await page.getByRole('button', { name: 'Retry' }).click()
  await expect(page.getByRole('list', { name: 'Allowance matrix' }).getByText('Alpha account')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Refresh all' })).toBeVisible()
  await expect(page.getByText(/^Updated /)).toBeVisible()
})

const NOW = Date.parse('2026-09-02T12:00:00Z')
const HOUR = 3_600_000

function resetWindow(key: string, resetAt: number | undefined, condition = 'normal') {
  return {
    key,
    guarded: false,
    label: `Window ${key}`,
    kind: 'quota_window',
    used: { value: 10, unit: 'tokens' },
    remaining: { value: 90, unit: 'tokens' },
    limit: { value: 100, unit: 'tokens' },
    used_percent: 10,
    reset_at: resetAt,
    condition,
    forecast: { status: 'no_risk' },
  }
}

function timelineSnapshot() {
  return {
    ...freshSnapshot,
    allowances: [
      // 168 小时内 12 条，其中第一条 tight；超出 168 小时 1 条（tight）；已过去与未知各 1 条不入轴
      ...Array.from({ length: 12 }, (_, index) =>
        resetWindow(
          `w${String(index + 1).padStart(2, '0')}`,
          NOW + (index + 1) * 3 * HOUR,
          index === 0 ? 'tight' : 'normal',
        ),
      ),
      resetWindow('far', NOW + 10 * 24 * HOUR, 'tight'),
      resetWindow('past', NOW - HOUR),
      resetWindow('unknown', undefined),
    ],
    models: [],
  }
}

test('timeline previews 10 resets within 168 hours, expands to all future resets and follows filters', async ({
  page,
}) => {
  await page.clock.setFixedTime(new Date(NOW))
  await mockAllowances(page, [timelineSnapshot()])
  await page.goto('/allowances')

  const timeline = page.getByRole('list', { name: 'Reset timeline' })
  const entries = timeline.getByRole('button', { name: /^Locate / })
  await expect(entries).toHaveCount(10)
  await expect(timeline.getByRole('button', { name: /Window far/ })).toHaveCount(0)
  const expand = page.getByRole('button', { name: 'Expand all' })
  await expect(expand).toHaveAttribute('aria-expanded', 'false')

  await expand.click()
  // 全部未来重置：12 + 1；过去与未知从不入轴
  await expect(entries).toHaveCount(13)
  await expect(timeline.getByRole('button', { name: /Window far/ })).toHaveCount(1)
  await expect(timeline.getByRole('button', { name: /Window past/ })).toHaveCount(0)
  await expect(timeline.getByRole('button', { name: /Window unknown/ })).toHaveCount(0)
  // 矩阵不受时间轴截断
  const provider = page.getByTestId('allowance-provider-provider-alpha')
  await expect(provider.getByText('Window w12', { exact: true })).toBeVisible()
  await expect(provider.getByText('Window past', { exact: true })).toBeVisible()
  await expect(provider.getByText('Window unknown', { exact: true })).toBeVisible()

  // 展开全部仍服从筛选，且展开状态跨筛选保留
  await selectFilter(page, 'All allowance states', 'Tight')
  await expect(entries).toHaveCount(2)
  await expect(page.getByRole('button', { name: 'Collapse', exact: true })).toHaveAttribute('aria-expanded', 'true')
  await selectFilter(page, 'Allowance state: Tight', 'All allowance states')
  await expect(entries).toHaveCount(13)

  await page.getByRole('button', { name: 'Collapse', exact: true }).click()
  await expect(entries).toHaveCount(10)
})

test('keeps provider details and timeline expansion across filters and refreshes without extra requests', async ({
  page,
}) => {
  await page.clock.setFixedTime(new Date(NOW))
  const snapshots = structuredClone([timelineSnapshot(), staleSnapshot])
  const posts = await mockAllowances(page, snapshots)
  await page.goto('/allowances')

  const alpha = page.getByTestId('allowance-provider-provider-alpha')
  await alpha.getByRole('button', { name: 'Alpha account allowance details' }).click()
  const details = alpha.getByRole('table', { name: 'Alpha account allowance details' })
  await expect(details).toBeVisible()
  // 账号标识等元数据只在展开详情中
  await expect(alpha.getByText('alpha@example.test')).toBeVisible()
  await page.getByRole('button', { name: 'Expand all' }).click()
  const timeline = page.getByRole('list', { name: 'Reset timeline' })
  await expect(timeline.getByRole('button', { name: /^Locate / })).toHaveCount(14)
  const requests: string[] = []
  page.on('request', (request) => requests.push(request.url()))

  // 筛选使该供应商离开再回来，展开状态保留
  await page.getByLabel('Search providers').fill('beta')
  await expect(alpha).toHaveCount(0)
  await page.getByLabel('Search providers').fill('')
  await expect(alpha.getByRole('table', { name: 'Alpha account allowance details' })).toBeVisible()
  await selectFilter(page, 'All provider types', 'openai-codex / codex')
  await expect(page.getByTestId('allowance-provider-provider-beta')).toHaveCount(0)
  await expect(alpha.getByRole('table', { name: 'Alpha account allowance details' })).toBeVisible()
  await selectFilter(page, 'Provider type: openai-codex / codex', 'All provider types')

  // 搜索也命中供应商类型与渠道
  await page.getByLabel('Search providers').fill('openai-codex')
  await expect(alpha).toBeVisible()
  await expect(page.getByTestId('allowance-provider-provider-beta')).toHaveCount(0)
  await page.getByLabel('Search providers').fill('')

  // 展开与筛选不产生请求
  expect(requests.filter((url) => url.includes('provider-allowances'))).toEqual([])

  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect(alpha.getByRole('table', { name: 'Alpha account allowance details' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Collapse', exact: true })).toHaveAttribute('aria-expanded', 'true')
})

test('filters always name their dimension and stay reachable on narrow viewports', async ({ page }) => {
  await mockAllowances(page, [freshSnapshot, staleSnapshot, errorSnapshot])
  await page.goto('/allowances')
  for (const name of ['All provider types', 'All allowance states', 'All data states']) {
    await expect(page.getByRole('button', { name, exact: true })).toBeVisible()
  }
  await selectFilter(page, 'All data states', 'Stale')
  await expect(page.getByRole('button', { name: 'Data state: Stale', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'All provider types', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'All allowance states', exact: true })).toBeVisible()
})

for (const width of [390, 320]) {
  test(`narrow Chinese layout at ${width}px keeps filter dimensions, quotas and pause reasons readable`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 800 })
    await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
    await page.clock.setFixedTime(new Date('2026-09-02T12:00:00Z'))
    await mockAllowances(page, [
      {
        ...freshSnapshot,
        suspension: {
          suspended_at: '2026-09-01T12:00:00Z',
          triggered_keys: ['weekly'],
          earliest_reset_at: 1788883200000,
        },
      },
      staleSnapshot,
    ])
    await page.goto('/allowances')

    for (const name of ['全部供应商类型', '全部额度状态', '全部数据状态']) {
      const filter = page.getByRole('button', { name, exact: true })
      await expect(filter).toBeVisible()
      const box = (await filter.boundingBox())!
      expect(box.x).toBeGreaterThanOrEqual(0)
      expect(box.x + box.width).toBeLessThanOrEqual(width)
    }
    // 首批额度不被汇总埋在折叠线以下
    const matrix = page.getByRole('list', { name: '额度矩阵' })
    const alpha = matrix.getByTestId('allowance-provider-provider-alpha')
    await expect(alpha.getByText('每周窗口', { exact: true })).toBeVisible()
    const firstQuota = (await alpha.getByText('每周窗口', { exact: true }).boundingBox())!
    expect(firstQuota.y).toBeLessThan(800)
    // 暂停原因通过暂停提示直接可达，且点击不会展开该行
    const pause = alpha.getByTestId('allowance-suspension')
    await pause.click()
    await expect(page.getByRole('tooltip')).toContainText('触发条目：weekly')
    await expect(alpha.getByRole('button', { name: 'Alpha account 额度详情' })).toHaveAttribute(
      'aria-expanded',
      'false',
    )
    await page.keyboard.press('Escape')
    await expect(page.getByRole('tooltip')).toHaveCount(0)
    // 时间轴定位可达
    const locate = page.getByRole('list', { name: '重置时间轴' }).getByRole('button').first()
    await locate.scrollIntoViewIfNeeded()
    await locate.click()
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    )
    expect(overflow).toBeLessThanOrEqual(0)
  })
}

test('uses wide workspace for more aligned quota tracks with a bounded rail', async ({ page }) => {
  const wide = {
    ...freshSnapshot,
    allowances: Array.from({ length: 9 }, (_, index) => resetWindow(`q${index + 1}`, 1788883200000 + index * HOUR)),
    models: [],
  }
  await mockAllowances(page, [wide])
  const tracks = async () =>
    page.getByTestId('allowance-provider-provider-alpha').evaluate((row) => {
      const items = [...row.querySelectorAll('ul > li[id^="allowance-"]')] as HTMLElement[]
      const firstTop = items[0].getBoundingClientRect().top
      const firstRow = items.filter((item) => Math.abs(item.getBoundingClientRect().top - firstTop) < 2)
      return {
        perRow: firstRow.length,
        maxWidth: Math.max(...items.map((item) => item.getBoundingClientRect().width)),
        meters: Math.max(
          ...[...row.querySelectorAll('[role="progressbar"]')].map((meter) => meter.getBoundingClientRect().width),
        ),
      }
    })

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/allowances')
  await expect(page.getByTestId('allowance-provider-provider-alpha')).toBeVisible()
  const narrow = await tracks()

  await page.setViewportSize({ width: 2560, height: 1440 })
  await expect.poll(async () => (await tracks()).perRow).toBeGreaterThan(narrow.perRow)
  const wideTracks = await tracks()
  // 轨道与进度条保持有界，新增宽度只用于容纳更多额度
  expect(wideTracks.maxWidth).toBeLessThanOrEqual(9 * 16 + 1)
  expect(wideTracks.meters).toBeLessThanOrEqual(14 * 4 + 1)
  const rail = page.getByRole('region', { name: 'Reset timeline' })
  const railBox = (await rail.boundingBox())!
  expect(railBox.width).toBeLessThanOrEqual(28 * 16 + 1)
  expect(railBox.width).toBeGreaterThanOrEqual(24 * 16 - 1)
  const matrixBox = (await page.getByRole('region', { name: 'Allowance matrix' }).boundingBox())!
  expect(matrixBox.width).toBeGreaterThan(railBox.width * 2)
})

test('keeps the matrix and side panels usable on a narrow Chinese viewport', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 760 })
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  await mockAllowances(page, [freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: '额度总览' })).toBeVisible()
  await expect(page.getByRole('button', { name: '全部刷新' })).toBeVisible()
  const matrix = page.getByRole('list', { name: '额度矩阵' })
  await expect(matrix.getByText('Alpha account')).toBeVisible()
  await matrix.getByRole('button', { name: 'Alpha account 额度详情' }).click()
  await expect(matrix.getByRole('switch', { name: '每周窗口耗尽时暂停此供应商' })).toBeVisible()
  await expect(page.getByRole('heading', { name: '重置时间轴' })).toBeVisible()
  await expect(page.getByRole('heading', { name: '预计耗尽' })).toBeVisible()
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  )
  expect(overflow).toBeLessThanOrEqual(0)
})
