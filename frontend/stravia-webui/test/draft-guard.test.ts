import { describe, expect, test } from 'bun:test'

import { DraftGuardController } from '../src/lib/draft-guard'

function setup(initial: { dirty?: boolean; busy?: boolean } = {}) {
  const state = { dirty: initial.dirty ?? false, busy: initial.busy ?? false }
  const confirmations: boolean[] = []
  const guard = new DraftGuardController({ dirty: () => state.dirty, busy: () => state.busy }, (confirming) =>
    confirmations.push(confirming),
  )
  return { state, guard, confirmations }
}

function navigation(willUnload = false) {
  const nav = {
    willUnload,
    cancelled: false,
    cancel() {
      nav.cancelled = true
    },
  }
  return nav
}

describe('draft guard close paths', () => {
  test('closes immediately without a draft', () => {
    const { guard, confirmations } = setup()
    let closed = 0
    expect(guard.request(() => closed++)).toBe('run')
    expect(closed).toBe(1)
    expect(guard.confirming).toBe(false)
    expect(confirmations).toEqual([])
  })

  test('asks first with a draft and closes only after the discard is confirmed', async () => {
    const { guard } = setup({ dirty: true })
    let closed = 0
    expect(guard.request(() => closed++)).toBe('confirm')
    expect(closed).toBe(0)
    expect(guard.confirming).toBe(true)

    await guard.confirmDiscard()
    expect(closed).toBe(1)
    expect(guard.confirming).toBe(false)
  })

  test('keeps the draft and drops the pending close when editing continues', async () => {
    const { guard } = setup({ dirty: true })
    let closed = 0
    guard.request(() => closed++)
    guard.keepEditing()
    await guard.confirmDiscard()
    expect(closed).toBe(0)
    expect(guard.confirming).toBe(false)
  })

  test('clears the confirmation before running the confirmed action', async () => {
    const { guard } = setup({ dirty: true })
    let confirmingDuringAction: boolean | undefined
    guard.request(() => {
      confirmingDuringAction = guard.confirming
    })
    await guard.confirmDiscard()
    await guard.confirmDiscard()
    expect(confirmingDuringAction).toBe(false)
  })

  test('locks every close path while saving', () => {
    const { guard, state } = setup({ dirty: true, busy: true })
    let closed = 0
    expect(guard.request(() => closed++)).toBe('blocked')
    expect(guard.confirming).toBe(false)
    state.dirty = false
    expect(guard.request(() => closed++)).toBe('blocked')
    expect(closed).toBe(0)
  })

  test('a failed save leaves the draft dirty so the next close still asks', () => {
    const { guard, state } = setup({ dirty: true })
    state.busy = true
    expect(guard.request(() => undefined)).toBe('blocked')
    state.busy = false // 保存失败：busy 结束，dirty 不变
    expect(guard.request(() => undefined)).toBe('confirm')
  })
})

describe('draft guard route leave', () => {
  test('does not interfere without a draft', () => {
    const { guard } = setup()
    const nav = navigation()
    guard.guardNavigation(nav, () => undefined)
    expect(nav.cancelled).toBe(false)
    expect(guard.confirming).toBe(false)
  })

  test('cancels, asks, and replays the navigation exactly once after confirmation', async () => {
    const { guard } = setup({ dirty: true })
    const first = navigation()
    let replayed = 0
    guard.guardNavigation(first, () => {
      replayed++
    })
    expect(first.cancelled).toBe(true)
    expect(guard.confirming).toBe(true)

    await guard.confirmDiscard()
    expect(replayed).toBe(1)

    // 重放发起的导航放行一次。
    const replay = navigation()
    guard.guardNavigation(replay, () => undefined)
    expect(replay.cancelled).toBe(false)

    // 之后恢复保护。
    const later = navigation()
    guard.guardNavigation(later, () => undefined)
    expect(later.cancelled).toBe(true)
  })

  test('clears the one-shot allowance when the replayed goto settles', async () => {
    const { guard } = setup({ dirty: true })
    guard.guardNavigation(navigation(), () => Promise.resolve())
    await guard.confirmDiscard()

    const later = navigation()
    guard.guardNavigation(later, () => undefined)
    expect(later.cancelled).toBe(true)
  })

  test('keeping the draft drops the replay', async () => {
    const { guard } = setup({ dirty: true })
    let replayed = 0
    guard.guardNavigation(navigation(), () => {
      replayed++
    })
    guard.keepEditing()
    await guard.confirmDiscard()
    expect(replayed).toBe(0)
  })

  test('page unload is cancelled for the native prompt without opening the in-page confirmation', () => {
    const { guard } = setup({ dirty: true })
    const nav = navigation(true)
    guard.guardNavigation(nav, () => undefined)
    expect(nav.cancelled).toBe(true)
    expect(guard.confirming).toBe(false)
  })

  test('navigation while saving is cancelled without a confirmation', () => {
    const { guard } = setup({ dirty: true, busy: true })
    const nav = navigation()
    guard.guardNavigation(nav, () => undefined)
    expect(nav.cancelled).toBe(true)
    expect(guard.confirming).toBe(false)
  })

  test('a navigation that cannot be replayed is only cancelled', () => {
    const { guard } = setup({ dirty: true })
    const nav = navigation()
    guard.guardNavigation(nav)
    expect(nav.cancelled).toBe(true)
    expect(guard.confirming).toBe(false)
  })
})
