import { describe, expect, test } from 'bun:test'

import {
  openaiCompatibleSupportsToggle,
  thinkingControlWritable,
  unrepresentableThinkingLevels,
  writableThinkingControlKinds,
} from '$lib/thinking-control'
import type { ThinkingLevelMapping } from '$lib/types'

const toggleMap: ThinkingLevelMapping[] = [
  { level: 'off', control: { type: 'disabled' }, source: 'generated' },
  { level: 'minimal', control: { type: 'hidden' }, source: 'generated' },
  { level: 'low', control: { type: 'hidden' }, source: 'generated' },
  { level: 'medium', control: { type: 'enabled' }, source: 'generated' },
  { level: 'high', control: { type: 'hidden' }, source: 'generated' },
  { level: 'xhigh', control: { type: 'hidden' }, source: 'generated' },
  { level: 'max', control: { type: 'hidden' }, source: 'generated' },
]

describe('thinking control representability', () => {
  test('openai-compatible without a known toggle shape only writes effort', () => {
    const context = { protocol: 'openai-compatible', presetKey: 'ollama-cloud', model: 'gemma4:31b' }

    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    expect(unrepresentableThinkingLevels(toggleMap, context)).toEqual(['off', 'medium'])
    expect(thinkingControlWritable({ type: 'effort', value: 'none' }, context)).toBe(true)
  })

  test('xiaomi can write the generated toggle map over openai-compatible', () => {
    const context = { protocol: 'openai-compatible', vendor: 'xiaomi', model: 'mimo-v2.5' }

    expect(openaiCompatibleSupportsToggle(context)).toBe(true)
    expect(unrepresentableThinkingLevels(toggleMap, context)).toEqual([])
    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'enabled', 'disabled', 'hidden'])
  })

  test('open-responses accepts disabled but not budget or enabled', () => {
    const context = { protocol: 'open-responses', model: 'gpt-5' }

    expect(thinkingControlWritable({ type: 'disabled' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'enabled' }, context)).toBe(false)
    expect(thinkingControlWritable({ type: 'budget', value: 1024 }, context)).toBe(false)
  })
})
