import { describe, expect, test } from 'bun:test'
import { QueryClient } from '@tanstack/svelte-query'

import {
  ProviderAllowanceReadController,
  type ProviderAllowanceReadApi,
  type ProviderAllowanceReadSnapshot,
} from '../src/lib/provider-allowance-read'
import type { Provider, ProviderAllowanceSnapshot, ProviderAllowanceTarget } from '../src/lib/types'

interface Deferred<T> {
  promise: Promise<T>
  resolve(value: T): void
  reject(reason?: unknown): void
}

function target(id: string): ProviderAllowanceTarget {
  return {
    provider_id: id,
    provider_name: id,
    catalog_provider_id: 'test-vendor',
    channel: 'default',
    guard_supported: true,
    suspension: null,
    refreshing: false,
  }
}

function allowance(id: string, plan = 'confirmed'): ProviderAllowanceSnapshot {
  const fields = target(id)
  return {
    ...fields,
    plan_label: plan,
    status: 'fresh',
    missing_guarded_keys: ['missing'],
    allowances: [
      {
        key: 'quota',
        guarded: true,
        label: 'Quota',
        kind: 'quota_window',
        remaining: { value: 10, unit: 'requests' },
        forecast: { status: 'no_risk' },
      },
    ],
    models: [],
  }
}

function invalidAllowance(id: string): ProviderAllowanceSnapshot {
  return {
    ...allowance(id),
    status: 'error',
    error: {
      category: 'authentication',
      message: 'Credential invalid; allowance fetching is paused until the credential is updated.',
    },
  }
}

function harness(
  ids: string[],
  overrides: Partial<ProviderAllowanceReadApi> = {},
  staleTime = 0,
  storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>,
) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime } } })
  const calls = { reads: [] as string[], refreshes: [] as string[], guards: [] as { id: string; keys: string[] }[] }
  const deferreds: { finish(): void }[] = []
  function deferred<T>(fallback: T) {
    const request = Promise.withResolvers<T>()
    deferreds.push({ finish: () => request.resolve(fallback) })
    return request
  }
  const providers: Array<Pick<Provider, 'id' | 'credential_status'>> = ids.map((id) => ({
    id,
    credential_status: 'ok',
  }))
  const listeners = new Set<() => void>()
  const guardInvoked = Promise.withResolvers<void>()
  const batchInvoked = Promise.withResolvers<void>()
  let notifications = 0
  const controller = new ProviderAllowanceReadController({
    client,
    storage,
    now: () => 4_000_000_000_000,
    api: {
      providers: overrides.providers ?? (() => Promise.resolve(providers)),
      list: overrides.list ?? (() => Promise.resolve(ids.map(target))),
      get: (id) => {
        calls.reads.push(id)
        return overrides.get?.(id) ?? Promise.resolve(allowance(id))
      },
      refresh: (id) => {
        calls.refreshes.push(id)
        if (calls.refreshes.length === ids.length) batchInvoked.resolve()
        return overrides.refresh?.(id) ?? Promise.resolve(allowance(id, 'refreshed'))
      },
      reset: overrides.reset ?? (() => Promise.resolve({ outcome: 'reset' })),
      replaceGuards: (id, keys) => {
        calls.guards.push({ id, keys })
        guardInvoked.resolve()
        return (
          overrides.replaceGuards?.(id, keys) ?? Promise.resolve({ keys, target: { ...target(id), refreshing: true } })
        )
      },
    },
    onSnapshot: () => {
      notifications += 1
      for (const listener of [...listeners]) listener()
    },
  })
  function waitFor(predicate: (snapshot: ProviderAllowanceReadSnapshot) => boolean): Promise<void> {
    if (predicate(controller.snapshot)) return Promise.resolve()
    const signal = Promise.withResolvers<void>()
    const listener = () => {
      if (!predicate(controller.snapshot)) return
      listeners.delete(listener)
      signal.resolve()
    }
    listeners.add(listener)
    return signal.promise
  }
  function entry(id: string) {
    const result = controller.snapshot.entries.find((entry) => entry.target.provider_id === id)
    if (!result) throw new Error(`Missing allowance entry ${id}`)
    return result
  }
  async function ready() {
    controller.start()
    await waitFor(
      (snapshot) =>
        snapshot.entries.length === ids.length && snapshot.entries.every((entry) => entry.snapshot && !entry.pending),
    )
  }
  async function cleanup() {
    controller.dispose()
    for (const request of deferreds) request.finish()
    await client.cancelQueries()
    client.clear()
  }
  return {
    client,
    controller,
    calls,
    deferred,
    waitFor,
    entry,
    ready,
    cleanup,
    guardInvoked,
    batchInvoked,
    notifications: () => notifications,
  }
}

describe('provider allowance reset', () => {
  function resetAllowance(id: string): ProviderAllowanceSnapshot {
    return {
      ...allowance(id),
      resets: {
        remaining: 2,
        grants: [
          { id: 'grant', label: 'Grant', remaining: 1, usable: true, expires_at: null, unavailable_reason: null },
          { id: 'other', label: 'Other', remaining: 1, usable: true, expires_at: null, unavailable_reason: null },
        ],
      },
    }
  }

  function storage() {
    const values = new Map<string, string>()
    return {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => {
        values.set(key, value)
      },
      removeItem: (key: string) => {
        values.delete(key)
      },
    }
  }

  test('blocks double submission and reports success separately from refresh failure', async () => {
    const pending = Promise.withResolvers<{ outcome: 'reset' }>()
    const invoked = Promise.withResolvers<void>()
    const refreshError = new Error('refresh failed')
    const requests: string[] = []
    const h = harness(
      ['one'],
      {
        get: (id) => Promise.resolve(resetAllowance(id)),
        reset: async (_id, request) => {
          requests.push(request.request_id)
          invoked.resolve()
          return pending.promise
        },
        refresh: () => Promise.reject(refreshError),
      },
      0,
      storage(),
    )
    try {
      await h.ready()
      await h.waitFor(() => h.entry('one').canReset)
      const first = h.controller.resetProvider('one', 'grant')
      await invoked.promise
      expect(h.entry('one').resetting).toBe(true)
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'ignored' })
      pending.resolve({ outcome: 'reset' })
      expect(await first).toEqual({ status: 'completed', outcome: 'reset', refreshError })
      expect(requests).toHaveLength(1)
      expect(h.calls.refreshes).toEqual(['one'])
      expect(h.entry('one').canReset).toBe(false)
    } finally {
      await h.cleanup()
    }
  })

  test('does not submit stale, unsupported, unusable or unpersisted resets', async () => {
    const requests: string[] = []
    const h = harness(
      ['one'],
      {
        get: (id) => Promise.resolve(resetAllowance(id)),
        reset: (_id, request) => {
          requests.push(request.request_id)
          return Promise.resolve({ outcome: 'reset' })
        },
      },
      0,
      {
        getItem: () => null,
        setItem: () => {
          throw new Error('storage unavailable')
        },
        removeItem: () => {},
      },
    )
    try {
      await h.ready()
      await h.waitFor(() => h.entry('one').canReset)
      const snapshot = resetAllowance('one')
      h.client.setQueryData(['provider-allowance', 'one'], { ...snapshot, status: 'stale' })
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'ignored' })
      h.client.setQueryData(['provider-allowance', 'one'], { ...snapshot, resets: undefined })
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'ignored' })
      h.client.setQueryData(['provider-allowance', 'one'], {
        ...snapshot,
        resets: { remaining: 1, grants: [{ ...snapshot.resets!.grants[0], usable: false }] },
      })
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'ignored' })
      h.client.setQueryData(['provider-allowance', 'one'], snapshot)
      const result = await h.controller.resetProvider('one', 'grant')
      expect(result.status).toBe('failed')
      expect(requests).toEqual([])
      expect(h.calls.refreshes).toEqual([])
    } finally {
      await h.cleanup()
    }
  })

  test('keeps the pending request after an unknown retry and retries it in the same controller', async () => {
    const saved = storage()
    const requests: Array<{ grant_id: string; request_id: string; retry?: boolean }> = []
    const timeout = new Error('timeout')
    const unknown = Object.assign(new Error('unknown reset request'), {
      code: 'PROVIDER_ALLOWANCE_RESET_UNKNOWN_REQUEST',
    })
    const h = harness(
      ['one'],
      {
        get: (id) => Promise.resolve(resetAllowance(id)),
        refresh: (id) => Promise.resolve(resetAllowance(id)),
        reset: (_id, request) => {
          requests.push(request)
          return Promise.reject(requests.length === 1 ? timeout : unknown)
        },
      },
      0,
      saved,
    )
    try {
      await h.ready()
      await h.waitFor(() => h.entry('one').canReset)
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'failed', error: timeout })
      const pending = saved.getItem('provider-allowance-reset:one')
      expect(pending).not.toBeNull()
      await h.waitFor(() => h.entry('one').canReset)
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'failed', error: unknown })
      expect(saved.getItem('provider-allowance-reset:one')).toBe(pending)
      expect(h.entry('one').pendingResetGrantId).toBe('grant')
      await h.waitFor(() => h.entry('one').canReset)
      expect(await h.controller.resetProvider('one', 'other')).toEqual({ status: 'ignored' })
      expect(await h.controller.resetProvider('one', 'grant')).toEqual({ status: 'failed', error: unknown })
      expect(saved.getItem('provider-allowance-reset:one')).toBe(pending)
      expect(requests).toHaveLength(3)
      expect(requests.map((request) => request.retry)).toEqual([false, true, true])
      expect(requests.map((request) => request.request_id)).toEqual([
        requests[0].request_id,
        requests[0].request_id,
        requests[0].request_id,
      ])
      expect(requests.map((request) => request.grant_id)).toEqual(['grant', 'grant', 'grant'])
    } finally {
      await h.cleanup()
    }
  })

  test('reuses an uncertain request across controller instances and blocks another grant', async () => {
    const saved = storage()
    const requests: Array<{ request_id: string; retry?: boolean }> = []
    const error = new Error('timeout')
    const api: Partial<ProviderAllowanceReadApi> = {
      get: (id) => Promise.resolve(resetAllowance(id)),
      refresh: (id) => Promise.resolve(resetAllowance(id)),
      reset: (_id, request) => {
        requests.push(request)
        if (requests.length === 1) return Promise.reject(error)
        return Promise.resolve({ outcome: 'already_used' })
      },
    }
    const first = harness(['one'], api, 0, saved)
    try {
      await first.ready()
      await first.waitFor(() => first.entry('one').canReset)
      expect(await first.controller.resetProvider('one', 'grant')).toEqual({ status: 'failed', error })
      expect(first.entry('one').pendingResetGrantId).toBe('grant')
    } finally {
      await first.cleanup()
    }
    const second = harness(
      ['one'],
      { ...api, get: (id) => Promise.resolve({ ...resetAllowance(id), resets: { remaining: 0, grants: [] } }) },
      0,
      saved,
    )
    try {
      await second.ready()
      await second.waitFor(() => second.entry('one').canReset)
      expect(second.entry('one').pendingResetGrantId).toBe('grant')
      expect(await second.controller.resetProvider('one', 'other')).toEqual({ status: 'ignored' })
      expect(await second.controller.resetProvider('one', 'grant')).toEqual({
        status: 'completed',
        outcome: 'already_used',
      })
      expect(requests).toHaveLength(2)
      expect(requests[0].retry).toBe(false)
      expect(requests[1].retry).toBe(true)
      expect(requests[1].request_id).toBe(requests[0].request_id)
      expect(requests[0].request_id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i)
      expect(second.entry('one').pendingResetGrantId).toBeUndefined()
    } finally {
      await second.cleanup()
    }
  })
})

describe('provider allowance reads', () => {
  test('invalid credentials pause only that provider and newer credential evidence resumes reads', async () => {
    const h = harness(['invalid', 'healthy'], {
      providers: () =>
        Promise.resolve([
          { id: 'invalid', credential_status: 'invalid' },
          { id: 'healthy', credential_status: 'ok' },
        ]),
    })
    try {
      h.controller.start()
      await h.waitFor(
        (snapshot) =>
          snapshot.entries.length === 2 && h.entry('invalid').credentialInvalid && !!h.entry('healthy').snapshot,
      )
      expect(h.calls.reads).toEqual(['healthy'])
      expect(await h.controller.refreshProvider('invalid')).toEqual({ status: 'ignored' })
      expect(await h.controller.refreshAll()).toEqual({ status: 'refreshed' })
      expect(h.calls.refreshes).toEqual(['healthy'])
      h.client.setQueryData(
        ['providers'],
        [
          { id: 'invalid', credential_status: 'ok' },
          { id: 'healthy', credential_status: 'ok' },
        ],
        { updatedAt: 4_000_000_000_001 },
      )
      await h.waitFor(() => !h.entry('invalid').credentialInvalid && h.entry('invalid').snapshot?.status === 'fresh')
      expect(h.calls.reads).toEqual(['healthy', 'invalid'])
      expect(h.entry('healthy').snapshot?.plan_label).toBe('refreshed')
    } finally {
      await h.cleanup()
    }
  })

  test('an invalid allowance response is remembered until newer provider evidence arrives', async () => {
    let invalidReads = 0
    const h = harness(['a', 'b'], {
      get: (id) =>
        Promise.resolve(
          id === 'a' && ++invalidReads === 1 ? invalidAllowance(id) : allowance(id, 'credential repaired'),
        ),
    })
    try {
      await h.ready()
      expect(h.entry('a').credentialInvalid).toBe(true)
      await h.controller.retryProvider('a')
      expect(await h.controller.refreshProvider('a')).toEqual({ status: 'ignored' })
      expect(h.calls.reads).toEqual(['a', 'b'])
      expect(await h.controller.refreshProvider('b')).toEqual({ status: 'refreshed' })
      h.client.setQueryData(
        ['providers'],
        [
          { id: 'a', credential_status: 'ok' },
          { id: 'b', credential_status: 'ok' },
        ],
        { updatedAt: 4_000_000_000_001 },
      )
      await h.waitFor(
        () => !h.entry('a').credentialInvalid && h.entry('a').snapshot?.plan_label === 'credential repaired',
      )
      expect(h.calls.reads.filter((id) => id === 'a')).toEqual(['a', 'a'])
      expect(h.calls.refreshes).toEqual(['b'])
    } finally {
      await h.cleanup()
    }
  })

  test('guard saves and manual refresh exclude each other per provider without blocking another provider', async () => {
    const saved = allowance('a', 'saved')
    saved.allowances[0].guarded = false
    const h = harness(['a', 'b'], {
      replaceGuards: () => save.promise,
      refresh: (id) => (id === 'a' ? refresh.promise : Promise.resolve(allowance(id, 'other refreshed'))),
    })
    const save = h.deferred({ keys: ['missing'], target: { ...target('a'), snapshot: saved, refreshing: true } })
    const refresh = h.deferred(allowance('a', 'refreshed'))
    try {
      await h.ready()
      const saving = h.controller.setGuard('a', 'quota', false)
      await h.guardInvoked.promise
      expect(await h.controller.refreshProvider('a')).toEqual({ status: 'ignored' })
      expect(await h.controller.refreshProvider('b')).toEqual({ status: 'refreshed' })
      save.resolve({ keys: ['missing'], target: { ...target('a'), snapshot: saved, refreshing: true } })
      await saving
      const refreshing = h.controller.refreshProvider('a')
      await h.waitFor(() => h.entry('a').refreshing)
      await h.controller.setGuard('a', 'quota', false)
      expect(h.calls.guards).toHaveLength(1)
      refresh.resolve(allowance('a', 'refreshed'))
      expect(await refreshing).toEqual({ status: 'refreshed' })
      expect(h.entry('b').snapshot?.plan_label).toBe('other refreshed')
    } finally {
      await h.cleanup()
    }
  })

  test('guard edits preserve missing keys and a failed save preserves confirmed guards', async () => {
    const error = new Error('guard write rejected')
    const h = harness(['a'], { replaceGuards: () => Promise.reject(error) }, 10_000)
    try {
      await h.ready()
      await h.controller.setGuard('a', 'quota', false)
      expect(h.calls.guards).toEqual([{ id: 'a', keys: ['missing'] }])
      expect(h.entry('a').guardError).toBe(error)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual(['missing'])
      await h.controller.setGuard('a', 'new', true)
      expect(new Set(h.calls.guards[1].keys)).toEqual(new Set(['quota', 'missing', 'new']))
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      expect(h.calls.reads).toEqual(['a'])
    } finally {
      await h.cleanup()
    }
  })

  test('a cancelled late GET cannot overwrite saved guards or remember stale invalid credentials', async () => {
    let reads = 0
    const lateStarted = Promise.withResolvers<void>()
    const h = harness(['a'], {
      get: () => {
        reads += 1
        if (reads === 2) {
          lateStarted.resolve()
          return late.promise
        }
        return Promise.resolve(reads === 1 ? allowance('a') : saved)
      },
      replaceGuards: () => save.promise,
    })
    const late = h.deferred(invalidAllowance('a'))
    const saved = allowance('a', 'saved guards')
    saved.allowances[0].guarded = false
    const save = h.deferred({ keys: ['missing'], target: { ...target('a'), snapshot: saved, refreshing: true } })
    try {
      await h.ready()
      const retry = h.controller.retryProvider('a')
      await lateStarted.promise
      await h.waitFor(() => h.entry('a').refreshing)
      const saving = h.controller.setGuard('a', 'quota', false)
      await h.guardInvoked.promise
      save.resolve({ keys: ['missing'], target: { ...target('a'), snapshot: saved, refreshing: true } })
      await saving
      late.resolve(invalidAllowance('a'))
      await retry
      expect(h.entry('a').snapshot?.plan_label).toBe('saved guards')
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(false)
      expect(h.entry('a').credentialInvalid).toBe(false)
      expect(await h.controller.refreshProvider('a')).toEqual({ status: 'refreshed' })
      expect(h.calls.refreshes).toEqual(['a'])
    } finally {
      await h.cleanup()
    }
  })

  test('PUT confirmation unlocks editing while GET and metadata remain pending', async () => {
    let reads = 0
    let lists = 0
    const h = harness(['a'], {
      get: () => (++reads === 1 ? Promise.resolve(allowance('a')) : background.promise),
      list: () => (++lists === 1 ? Promise.resolve([target('a')]) : metadata.promise),
      replaceGuards: () => save.promise,
    })
    const save = h.deferred({ keys: ['missing'], target: { ...target('a'), refreshing: true } })
    const final = allowance('a', 'fresh evidence')
    final.allowances[0].guarded = false
    final.suspension = { suspended_at: '2026-09-01T12:00:00Z', triggered_keys: ['missing'], earliest_reset_at: null }
    const background = h.deferred(final)
    const metadata = h.deferred([{ ...target('a'), suspension: final.suspension, snapshot: final }])
    try {
      await h.ready()
      const saving = h.controller.setGuard('a', 'quota', false)
      await h.guardInvoked.promise
      expect(h.entry('a').savingGuards).toBe(true)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      save.resolve({ keys: ['missing'], target: { ...target('a'), refreshing: true } })
      await saving
      expect(h.entry('a').savingGuards).toBe(false)
      expect(h.entry('a').canSaveGuards).toBe(true)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(false)
      expect(h.entry('a').snapshot?.plan_label).toBe('confirmed')
      expect(h.entry('a').refreshing).toBe(true)
      background.resolve(final)
      await h.waitFor(() => h.entry('a').snapshot?.plan_label === 'fresh evidence')
      expect(h.entry('a').snapshot?.suspension).toEqual(final.suspension)
      expect(h.entry('a').canSaveGuards).toBe(true)
      expect(h.calls.refreshes).toEqual([])
      metadata.resolve([{ ...target('a'), suspension: final.suspension, snapshot: final }])
    } finally {
      await h.cleanup()
    }
  })

  test('successive saves retain the latest confirmed keys and suspension against late GET', async () => {
    let reads = 0
    const paused = allowance('a')
    paused.suspension = { suspended_at: '2026-09-01T12:00:00Z', triggered_keys: ['quota'], earliest_reset_at: null }
    const obsolete = { ...invalidAllowance('a'), suspension: paused.suspension }
    const h = harness(['a'], {
      get: () => (++reads === 1 ? Promise.resolve(paused) : reads === 2 ? firstRead.promise : lastRead.promise),
      replaceGuards: (_id, keys) =>
        Promise.resolve({
          keys: keys.filter((key) => key !== 'missing'),
          target: { ...target('a'), refreshing: true },
        }),
    })
    const firstRead = h.deferred(obsolete)
    const final = allowance('a', 'last evidence')
    final.allowances[0].guarded = false
    final.missing_guarded_keys = ['new']
    const lastRead = h.deferred(final)
    try {
      await h.ready()
      await h.controller.setGuard('a', 'quota', false)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual([])
      await h.controller.setGuard('a', 'new', true)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual(['new'])
      expect(h.calls.guards[1].keys).toEqual(['new'])
      firstRead.resolve(obsolete)
      await firstRead.promise
      expect(h.entry('a').credentialInvalid).toBe(false)
      expect(h.entry('a').snapshot?.suspension).toBeNull()
      expect(h.entry('a').target.suspension).toBeNull()
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual(['new'])
      expect(h.entry('a').canSaveGuards).toBe(true)
      lastRead.resolve(final)
      await h.waitFor(() => h.entry('a').snapshot?.plan_label === 'last evidence')
      expect(h.entry('a').credentialInvalid).toBe(false)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual(['new'])
    } finally {
      await h.cleanup()
    }
  })

  test('a failed second save resumes the first confirmed background read with production stale time', async () => {
    let reads = 0
    let saves = 0
    const initialRead = Promise.withResolvers<void>()
    const resumedRead = Promise.withResolvers<void>()
    const error = new Error('second guard write rejected')
    const final = allowance('a', 'fresh exhausted evidence')
    final.missing_guarded_keys = []
    final.allowances[0].remaining = { value: 0, unit: 'requests' }
    final.allowances[0].condition = 'exhausted'
    final.suspension = { suspended_at: '2026-09-01T12:00:00Z', triggered_keys: ['quota'], earliest_reset_at: null }
    const h = harness(
      ['a'],
      {
        get: () => {
          reads += 1
          if (reads === 1) return Promise.resolve(allowance('a'))
          if (reads === 2) initialRead.resolve()
          else resumedRead.resolve()
          return background.promise
        },
        replaceGuards: (_id, keys) =>
          ++saves === 1
            ? Promise.resolve({ keys, target: { ...target('a'), refreshing: true } })
            : Promise.reject(error),
      },
      10_000,
    )
    const background = h.deferred(final)
    try {
      await h.ready()
      await h.controller.setGuard('a', 'missing', false)
      await initialRead.promise
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual([])
      expect(h.entry('a').snapshot?.suspension).toBeNull()
      await h.controller.setGuard('a', 'quota', false)
      await resumedRead.promise
      expect(h.entry('a').guardError).toBe(error)
      expect(h.entry('a').canSaveGuards).toBe(true)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual([])
      background.resolve(final)
      await h.waitFor(() => h.entry('a').snapshot?.plan_label === 'fresh exhausted evidence')
      expect(h.entry('a').snapshot?.suspension).toEqual(final.suspension)
      expect(h.entry('a').snapshot?.allowances[0].remaining?.value).toBe(0)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(true)
      expect(h.entry('a').snapshot?.missing_guarded_keys).toEqual([])
      expect(h.entry('a').guardError).toBe(error)
      expect(h.calls.refreshes).toEqual([])
    } finally {
      await h.cleanup()
    }
  })

  test('background metadata failure is distinct from a successful guard write', async () => {
    let lists = 0
    let reads = 0
    const error = new Error('background metadata unavailable')
    const saved = allowance('a')
    saved.allowances[0].guarded = false
    const h = harness(['a'], {
      get: () => Promise.resolve(++reads === 1 ? allowance('a') : saved),
      list: () => (++lists === 1 ? Promise.resolve([target('a')]) : Promise.reject(error)),
    })
    try {
      await h.ready()
      await h.controller.setGuard('a', 'quota', false)
      await h.waitFor(() => h.entry('a').refreshError != null)
      expect(h.entry('a').guardError).toBeUndefined()
      expect(h.entry('a').refreshError).toBe(error)
      expect(h.entry('a').snapshot?.allowances[0].guarded).toBe(false)
      expect(h.entry('a').canSaveGuards).toBe(true)
    } finally {
      await h.cleanup()
    }
  })

  test('batch refresh publishes successes and reports the first failure in target order', async () => {
    const failures = [new Error('first target failed'), new Error('third target failed')]
    const requests: Record<string, Deferred<ProviderAllowanceSnapshot>> = {}
    const h = harness(['a', 'b', 'c'], { refresh: (id) => requests[id].promise })
    for (const id of ['a', 'b', 'c']) requests[id] = h.deferred(allowance(id))
    try {
      await h.ready()
      const refreshing = h.controller.refreshAll()
      await h.batchInvoked.promise
      requests.c.reject(failures[1])
      requests.b.resolve(allowance('b', 'batch success'))
      await h.waitFor(() => h.entry('b').snapshot?.plan_label === 'batch success')
      requests.a.reject(failures[0])
      expect(await refreshing).toEqual({ status: 'failed', error: failures[0] })
      expect(h.entry('a').snapshot?.plan_label).toBe('confirmed')
      expect(h.entry('b').snapshot?.plan_label).toBe('batch success')
      expect(h.entry('c').snapshot?.plan_label).toBe('confirmed')
      expect(h.controller.snapshot.refreshingAll).toBe(false)
    } finally {
      await h.cleanup()
    }
  })

  test('disposing during initialization, reads, or writes suppresses late view notifications and keeps shared cache', async () => {
    const initialization = Promise.withResolvers<ProviderAllowanceTarget[]>()
    const init = harness(['a'], { list: () => initialization.promise })
    let reads = 0
    const active = harness(['a', 'b'], {
      get: (id) => (id === 'a' && ++reads > 1 ? read.promise : Promise.resolve(allowance(id))),
      refresh: () => write.promise,
    })
    const read = active.deferred(allowance('a', 'late read'))
    const write = active.deferred(allowance('b', 'late write'))
    try {
      init.client.setQueryData(['shared-cache'], { retained: true })
      init.controller.start()
      init.controller.dispose()
      const initNotifications = init.notifications()
      initialization.resolve([target('a')])
      await initialization.promise
      await init.client.cancelQueries()
      expect(init.notifications()).toBe(initNotifications)
      expect(init.client.getQueryData<{ retained: boolean }>(['shared-cache'])).toEqual({ retained: true })
      expect(await init.controller.refreshAll()).toEqual({ status: 'ignored' })

      await active.ready()
      const retry = active.controller.retryProvider('a')
      await active.waitFor(() => active.entry('a').refreshing)
      const refreshing = active.controller.refreshProvider('b')
      await active.waitFor(() => active.entry('b').refreshing)
      active.controller.dispose()
      const notifications = active.notifications()
      read.resolve(allowance('a', 'late read'))
      write.resolve(allowance('b', 'late write'))
      await retry
      expect(await refreshing).toEqual({ status: 'ignored' })
      expect(active.notifications()).toBe(notifications)
      expect(await active.controller.refreshProvider('a')).toEqual({ status: 'ignored' })
      await active.controller.setGuard('a', 'quota', false)
      expect(active.calls.guards).toEqual([])
    } finally {
      initialization.resolve([target('a')])
      await init.cleanup()
      await active.cleanup()
    }
  })
})
