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

function harness(ids: string[], overrides: Partial<ProviderAllowanceReadApi> = {}) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: Infinity } } })
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
      replaceGuards: (id, keys) => {
        calls.guards.push({ id, keys })
        guardInvoked.resolve()
        return overrides.replaceGuards?.(id, keys) ?? Promise.resolve(allowance(id, 'saved'))
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
    const h = harness(['a', 'b'], {
      replaceGuards: () => save.promise,
      refresh: (id) => (id === 'a' ? refresh.promise : Promise.resolve(allowance(id, 'other refreshed'))),
    })
    const save = h.deferred(allowance('a', 'saved'))
    const refresh = h.deferred(allowance('a', 'refreshed'))
    try {
      await h.ready()
      const saving = h.controller.setGuard('a', 'quota', false)
      await h.guardInvoked.promise
      expect(await h.controller.refreshProvider('a')).toEqual({ status: 'ignored' })
      expect(await h.controller.refreshProvider('b')).toEqual({ status: 'refreshed' })
      save.resolve(allowance('a', 'saved'))
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
    const h = harness(['a'], { replaceGuards: () => Promise.reject(error) })
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
    } finally {
      await h.cleanup()
    }
  })

  test('a cancelled late GET cannot overwrite saved guards or remember stale invalid credentials', async () => {
    let reads = 0
    const h = harness(['a'], {
      get: () => (++reads === 2 ? late.promise : Promise.resolve(reads === 1 ? allowance('a') : saved)),
      replaceGuards: () => save.promise,
    })
    const late = h.deferred(invalidAllowance('a'))
    const saved = allowance('a', 'saved guards')
    saved.allowances[0].guarded = false
    const save = h.deferred(saved)
    try {
      await h.ready()
      const retry = h.controller.retryProvider('a')
      await h.waitFor(() => h.entry('a').refreshing)
      const saving = h.controller.setGuard('a', 'quota', false)
      await h.guardInvoked.promise
      save.resolve(saved)
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
      expect(active.client.getQueryData<ProviderAllowanceSnapshot>(['provider-allowance', 'a'])?.plan_label).toBe(
        'late read',
      )
      expect(active.client.getQueryData<ProviderAllowanceSnapshot>(['provider-allowance', 'b'])?.plan_label).toBe(
        'late write',
      )
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
