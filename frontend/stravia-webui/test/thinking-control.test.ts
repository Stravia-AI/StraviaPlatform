import { describe, expect, test } from 'bun:test'

import {
  openaiCompatibleSupportsToggle,
  thinkingControlWritable,
  unrepresentableThinkingLevels,
  writableThinkingControlKinds,
} from '#lib/thinking-control.js'
import type { ThinkingLevelMapping } from '#lib/types.js'

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
  test('Kiro Builder ID accepts the declared Sonnet 5.5 efforts without granting a toggle', () => {
    const levels = ['low', 'medium', 'high', 'xhigh', 'max'] as const
    const context = {
      protocol: 'kiro-generate-assistant-response-oauth',
      vendor: 'kiro',
      model: 'claude-sonnet-5.5',
      reasoningEfforts: levels,
    }
    const rows: ThinkingLevelMapping[] = levels.map((level) => ({
      level,
      control: { type: 'effort', value: level },
      source: 'generated',
    }))

    expect(unrepresentableThinkingLevels(rows, context)).toEqual([])
    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    expect(thinkingControlWritable({ type: 'effort', value: 'HIGH' }, context)).toBe(true)
    for (const value of ['', 'none', 'minimal']) {
      expect(thinkingControlWritable({ type: 'effort', value }, context)).toBe(false)
    }
    expect(thinkingControlWritable({ type: 'disabled' }, context)).toBe(false)
    expect(unrepresentableThinkingLevels(rows, { ...context, reasoningEfforts: null })).toEqual([...levels])
    for (const reasoningEfforts of [[], ['']]) {
      expect(writableThinkingControlKinds({ ...context, reasoningEfforts })).toEqual(['hidden'])
    }
  })

  test('a proprietary model can map off to a declared none effort without a toggle', () => {
    const context = { protocol: 'kiro-generate-assistant-response-oauth', reasoningEfforts: ['none', 'high'] }
    const off: ThinkingLevelMapping = { level: 'off', control: { type: 'effort', value: 'none' }, source: 'generated' }

    expect(unrepresentableThinkingLevels([off], context)).toEqual([])
    expect(thinkingControlWritable({ type: 'effort', value: 'off' }, context)).toBe(false)
    expect(thinkingControlWritable({ type: 'disabled' }, context)).toBe(false)
  })

  test('openai-compatible without a known toggle shape only writes effort', () => {
    const context = { protocol: 'openai-compatible', presetKey: 'ollama-cloud', model: 'gemma4:31b' }

    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    expect(unrepresentableThinkingLevels(toggleMap, context)).toEqual(['off', 'medium'])
    expect(thinkingControlWritable({ type: 'effort', value: 'none' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'effort', value: 'high' }, { ...context, reasoningEfforts: ['low'] })).toBe(
      true,
    )
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

  test('command-code only writes effort', () => {
    const context = { protocol: 'command-code', vendor: 'command-code', model: 'claude-sonnet-4-6' }

    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    expect(thinkingControlWritable({ type: 'effort', value: 'high' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'enabled' }, context)).toBe(false)
  })

  test('devin-connect writes effort through the selector suffix', () => {
    const context = { protocol: 'devin-connect', vendor: 'devin', model: 'swe-2' }

    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    expect(thinkingControlWritable({ type: 'effort', value: 'high' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'enabled' }, context)).toBe(false)
  })
})
