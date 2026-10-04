import { describe, expect, test } from 'bun:test'
import { ProviderModelEditingController } from '../src/lib/provider-model-editing'
import type {
  ProviderModelEditingApi,
  ProviderModelEditingDeps,
  ProviderModelEditingOperation,
} from '../src/lib/provider-model-editing'
import type { PreparedProviderModel, ProviderModelDetail } from '../src/lib/types'

function detail(id = 'A', revision = 1): ProviderModelDetail {
  return {
    id,
    revision,
    available: true,
    source_kind: 'manual',
    can_reimport: true,
    snapshot_state: { type: 'edited', source: null },
    selection_policy: 'auto',
    metadata: { id, name: `confirmed ${id}` },
    extensions: {},
    created_at: '',
    updated_at: '',
  }
}
function harness(overrides: Partial<ProviderModelEditingDeps> = {}, api: Partial<ProviderModelEditingApi> = {}) {
  const errors: Array<{ error: unknown; phase: string }> = []
  const writes: unknown[][] = []
  const navigations: Array<string | undefined> = []
  const refreshes: ProviderModelEditingOperation[] = []
  const successes: ProviderModelEditingOperation[] = []
  const controller = new ProviderModelEditingController({
    api: {
      model: (_provider, id) => Promise.resolve(detail(id)),
      prepareModel: (_provider, id) =>
        Promise.resolve({
          id,
          metadata: { name: 'prepared' },
          extensions: {},
          snapshot_state: { type: 'imported', source: { type: 'canonical', model_id: 'vendor/template' } },
        }),
      createManualModel: (...args) => {
        writes.push(args)
        return Promise.resolve(detail(args[1], 1))
      },
      updateModel: (...args) => {
        writes.push(args)
        return Promise.resolve(detail(args[1], args[3] + 1))
      },
      updateModelSelection: (...args) => {
        writes.push(args)
        return Promise.resolve({ ...detail(args[1], args[3] + 1), available: false, selection_policy: args[2] })
      },
      reimportModel: (...args) => {
        writes.push(args)
        return Promise.resolve(detail(args[1], args[2] + 1))
      },
      deleteManualModel: (...args) => {
        writes.push(args)
        return Promise.resolve()
      },
      ...api,
    },
    navigate: (id) => {
      navigations.push(id)
      return Promise.resolve()
    },
    refresh: (operation) => {
      refreshes.push(operation)
      return Promise.resolve()
    },
    onSuccess: (operation) => {
      successes.push(operation)
    },
    onError: (error, phase) => {
      errors.push({ error, phase })
    },
    settleEditor: () => Promise.resolve(),
    ...overrides,
  })
  return { controller, errors, writes, navigations, refreshes, successes }
}

describe('provider model editing', () => {
  test('late A success cannot replace B or finish its loading', async () => {
    const a = Promise.withResolvers<ProviderModelDetail>()
    const b = Promise.withResolvers<ProviderModelDetail>()
    const { controller } = harness({}, { model: (_provider, id) => (id === 'A' ? a.promise : b.promise) })
    const first = controller.select('provider', 'A')
    const second = controller.select('provider', 'B')
    expect(controller.snapshot.detail).toBeUndefined()
    a.resolve(detail('A'))
    await first
    expect(controller.snapshot.modelId).toBe('B')
    expect(controller.snapshot.loading).toBe(true)
    b.resolve(detail('B'))
    await second
    expect(controller.snapshot.detail?.id).toBe('B')
  })

  test('late rejection cannot replace B error or report stale failure', async () => {
    const a = Promise.withResolvers<ProviderModelDetail>()
    const b = Promise.withResolvers<ProviderModelDetail>()
    const { controller, errors } = harness({}, { model: (_provider, id) => (id === 'A' ? a.promise : b.promise) })
    const first = controller.select('provider', 'A')
    const second = controller.select('provider', 'B')
    const currentError = new Error('B failed')
    b.reject(currentError)
    await second
    a.reject(new Error('A failed'))
    await first
    expect(controller.snapshot.readError).toBe(currentError)
    expect(controller.snapshot.loading).toBe(false)
    expect(errors).toEqual([{ error: currentError, phase: 'read' }])
  })

  test('stale rejection does not stop a newer read still loading', async () => {
    const a = Promise.withResolvers<ProviderModelDetail>()
    const b = Promise.withResolvers<ProviderModelDetail>()
    const { controller, errors } = harness({}, { model: (_provider, id) => (id === 'A' ? a.promise : b.promise) })
    const first = controller.select('provider', 'A')
    const second = controller.select('provider', 'B')
    a.reject(new Error('stale'))
    await first
    expect(controller.snapshot.loading).toBe(true)
    expect(controller.snapshot.readError).toBeUndefined()
    expect(errors).toEqual([])
    b.resolve(detail('B'))
    await second
    expect(controller.snapshot.detail?.id).toBe('B')
  })

  for (const action of ['close', 'dispose'] as const) {
    test(`${action} invalidates an in-flight detail read`, async () => {
      const read = Promise.withResolvers<ProviderModelDetail>()
      const { controller, errors } = harness({}, { model: () => read.promise })
      const pending = controller.select('provider', 'A')
      controller[action]()
      read.resolve(detail())
      await pending
      expect(controller.snapshot.detail).toBeUndefined()
      expect(controller.snapshot.modelId).toBe('')
      expect(controller.snapshot.loading).toBe(false)
      expect(errors).toEqual([])
    })
  }

  test('dirty guard keeps one action; keep cancels and discard resets before acting exactly once', async () => {
    const settle = Promise.withResolvers<void>()
    const { controller } = harness({ settleEditor: () => settle.promise })
    await controller.select('provider', 'A')
    const originalMetadata = controller.snapshot.detail?.metadata
    controller.markDirty(true)
    let left = 0
    controller.requestLeave(() => {
      left += 1
    })
    controller.keepEditing()
    expect(controller.snapshot.dirty).toBe(true)
    expect(left).toBe(0)
    controller.requestLeave(() => {
      left += 1
    })
    controller.requestLeave(() => {
      left += 100
    })
    const discard = controller.confirmDiscard()
    controller.markDirty(true)
    expect(controller.snapshot.dirty).toBe(false)
    expect(controller.snapshot.detail?.metadata).not.toBe(originalMetadata)
    expect(controller.snapshot.detail?.metadata).toEqual(originalMetadata)
    expect(left).toBe(0)
    settle.resolve()
    await discard
    await controller.confirmDiscard()
    expect(left).toBe(1)
  })

  test('dirty selection cannot bypass discard and switches after confirmation', async () => {
    const { controller } = harness()
    await controller.select('provider', 'A')
    controller.markDirty(true)
    await controller.select('other-provider', 'B')
    expect(controller.snapshot.detail?.id).toBe('A')
    expect(controller.snapshot.discardOpen).toBe(true)
    await controller.confirmDiscard()
    expect(controller.snapshot.providerId).toBe('other-provider')
    expect(controller.snapshot.detail?.id).toBe('B')
  })

  test('write and its refresh lock editing, navigation, selection and duplicate writes', async () => {
    const write = Promise.withResolvers<ProviderModelDetail>()
    const refresh = Promise.withResolvers<void>()
    let count = 0
    const { controller, navigations } = harness(
      { refresh: () => refresh.promise },
      {
        updateModel: () => {
          count += 1
          return write.promise
        },
      },
    )
    await controller.select('provider', 'A')
    controller.markDirty(true)
    const saving = controller.save('{"name":"changed"}')
    await controller.save('{}')
    await controller.changeSelection('force_disabled')
    await controller.select('provider', 'B')
    controller.close()
    controller.requestLeave(() => {
      navigations.push('external')
    })
    controller.markDirty(false)
    expect(controller.snapshot.dirty).toBe(true)
    expect(controller.snapshot.modelId).toBe('A')
    expect(navigations).toEqual([])
    expect(count).toBe(1)
    write.resolve(detail('A', 2))
    await Promise.resolve()
    await Promise.resolve()
    expect(controller.snapshot.busy).toBe(true)
    controller.markDirty(true)
    expect(controller.snapshot.dirty).toBe(false)
    controller.close()
    refresh.resolve()
    await saving
    expect(controller.snapshot.busy).toBe(false)
    expect(controller.snapshot.detail?.revision).toBe(2)
  })

  test('availability preserves metadata identity and outstanding specification edits', async () => {
    const { controller, writes } = harness()
    await controller.select('provider', 'A')
    const metadata = controller.snapshot.detail?.metadata
    controller.markDirty(true)
    await controller.changeSelection('force_disabled')
    expect(controller.snapshot.detail?.metadata).toBe(metadata)
    expect(controller.snapshot.dirty).toBe(true)
    expect(controller.snapshot.detail?.selection_policy).toBe('force_disabled')
    expect(controller.snapshot.detail?.available).toBe(false)
    await controller.save('{"name":"unsaved specification"}')
    expect(writes[1]).toEqual(['provider', 'A', '{"name":"unsaved specification"}', 2])
  })

  test('write failure retains confirmed detail and dirty state', async () => {
    const failure = new Error('conflict')
    const { controller, errors, refreshes } = harness({}, { updateModel: () => Promise.reject(failure) })
    await controller.select('provider', 'A')
    const confirmed = controller.snapshot.detail
    controller.markDirty(true)
    await controller.save('{}')
    expect(controller.snapshot.detail).toBe(confirmed)
    expect(controller.snapshot.dirty).toBe(true)
    expect(controller.snapshot.busy).toBe(false)
    expect(refreshes).toEqual([])
    expect(errors).toEqual([{ error: failure, phase: 'write' }])
  })

  test('reimport waits for discard then writes the current confirmed revision', async () => {
    const { controller, writes, successes } = harness()
    await controller.select('provider', 'A')
    controller.markDirty(true)
    controller.requestReimport()
    expect(writes).toEqual([])
    controller.keepEditing()
    expect(controller.snapshot.dirty).toBe(true)
    controller.requestReimport()
    await controller.confirmDiscard()
    expect(writes).toEqual([['provider', 'A', 1]])
    expect(controller.snapshot.detail?.revision).toBe(2)
    expect(controller.snapshot.dirty).toBe(false)
    expect(successes).toEqual(['reimport'])
  })

  test('refresh failure is partial success and retry never repeats the write', async () => {
    const failure = new Error('refresh unavailable')
    let refreshCount = 0
    const { controller, writes, errors, successes } = harness({
      refresh: () => {
        refreshCount += 1
        return refreshCount === 1 ? Promise.reject(failure) : Promise.resolve()
      },
    })
    await controller.select('provider', 'A')
    controller.markDirty(true)
    await controller.save('{}')
    expect(controller.snapshot.detail?.revision).toBe(2)
    expect(controller.snapshot.dirty).toBe(false)
    expect(controller.snapshot.refreshError).toBe(failure)
    expect(successes).toEqual(['save'])
    expect(errors).toEqual([{ error: failure, phase: 'refresh' }])
    await controller.refresh()
    expect(writes).toEqual([['provider', 'A', '{}', 1]])
    expect(refreshCount).toBe(2)
    expect(controller.snapshot.refreshError).toBeUndefined()
  })

  test('manual prepare carries template and captured provider into creation and closes draft drawer', async () => {
    const { controller, writes, navigations } = harness()
    expect(await controller.prepareManual('provider', 'vendor/template', [])).toBe(true)
    expect(controller.snapshot.modelId).toBe('template')
    expect(controller.snapshot.draft).toBe(true)
    expect(controller.snapshot.drawerOpen).toBe(true)
    controller.markDirty(true)
    await controller.save('{"name":"manual"}')
    expect(writes).toEqual([['provider', 'template', '{"name":"manual"}', 'vendor/template']])
    expect(controller.snapshot.draft).toBe(false)
    expect(controller.snapshot.drawerOpen).toBe(false)
    expect(navigations).toEqual(['template'])
  })

  test('closing preparation prevents late draft and navigation resurrection', async () => {
    const prepared = Promise.withResolvers<PreparedProviderModel>()
    const { controller, navigations } = harness({}, { prepareModel: () => prepared.promise })
    const pending = controller.prepareManual('provider', 'vendor/template', [])
    controller.close()
    prepared.resolve({ id: 'template', metadata: {}, extensions: {}, snapshot_state: { type: 'edited', source: null } })
    expect(await pending).toBe(false)
    expect(controller.snapshot.detail).toBeUndefined()
    expect(controller.snapshot.preparing).toBe(false)
    expect(navigations).toEqual([undefined])
  })

  test('manual entry selecting an existing ID retains saved metadata and writes its authoritative revision', async () => {
    const existing = { ...detail('template', 17), metadata: { name: 'Saved specification', limit: { context: 12000 } } }
    const { controller, writes } = harness({}, { model: () => Promise.resolve(existing) })
    expect(await controller.prepareManual('provider', 'vendor/template', ['template'])).toBe(true)
    expect(controller.snapshot.draft).toBe(false)
    expect(controller.snapshot.detail?.metadata).toBe(existing.metadata)
    expect(controller.snapshot.detail?.metadata.name).toBe('Saved specification')
    expect(controller.snapshot.detail?.revision).toBe(17)
    await controller.save('{"name":"Explicit edit"}')
    expect(writes).toEqual([['provider', 'template', '{"name":"Explicit edit"}', 17]])
  })

  test('failed preparation keeps inventory selected and a corrected template can create a new draft', async () => {
    const failure = new Error('template removed')
    const { controller, errors } = harness(
      {},
      {
        prepareModel: (_provider, id) =>
          id === 'missing'
            ? Promise.reject(failure)
            : Promise.resolve({
                id,
                metadata: { name: 'Replacement' },
                extensions: {},
                snapshot_state: { type: 'edited', source: null },
              }),
      },
    )
    expect(await controller.prepareManual('provider', 'vendor/missing', [])).toBe(false)
    expect(controller.snapshot.modelId).toBe('')
    expect(controller.snapshot.detail).toBeUndefined()
    expect(controller.snapshot.preparing).toBe(false)
    expect(errors).toEqual([{ error: failure, phase: 'read' }])
    expect(await controller.prepareManual('provider', 'vendor/replacement', [])).toBe(true)
    expect(controller.snapshot.modelId).toBe('replacement')
    expect(controller.snapshot.detail?.metadata.name).toBe('Replacement')
    expect(controller.snapshot.draft).toBe(true)
    expect(controller.snapshot.readError).toBeUndefined()
  })

  test('manual removal cannot delete a discovered Provider Model', async () => {
    const discovered: ProviderModelDetail = { ...detail('A'), source_kind: 'discovered' }
    const { controller, writes, refreshes } = harness({}, { model: () => Promise.resolve(discovered) })
    await controller.select('provider', 'A')
    controller.requestDelete()
    await controller.delete()
    expect(controller.snapshot.detail).toBe(discovered)
    expect(controller.snapshot.modelId).toBe('A')
    expect(writes).toEqual([])
    expect(refreshes).toEqual([])
  })

  test('delete success cannot resurrect detail when refresh and navigation both fail', async () => {
    const refreshError = new Error('refresh')
    const navigationError = new Error('navigation')
    const { controller, writes, errors } = harness({
      refresh: () => Promise.reject(refreshError),
      navigate: () => Promise.reject(navigationError),
    })
    await controller.select('provider', 'A')
    controller.markDirty(true)
    controller.requestDelete()
    expect(controller.snapshot.deleteOpen).toBe(false)
    await controller.confirmDiscard()
    expect(controller.snapshot.deleteOpen).toBe(true)
    await controller.delete()
    expect(writes).toEqual([['provider', 'A']])
    expect(controller.snapshot.detail).toBeUndefined()
    expect(controller.snapshot.modelId).toBe('')
    expect(controller.snapshot.deleteOpen).toBe(false)
    expect(controller.snapshot.busy).toBe(false)
    expect(errors).toEqual([
      { error: refreshError, phase: 'refresh' },
      { error: navigationError, phase: 'navigation' },
    ])
  })
})
