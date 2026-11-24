import { describe, expect, test } from 'bun:test'

import {
  defaultEffortControl,
  effortCandidates,
  thinkingControlWritable,
  unrepresentableThinkingLevels,
  writableThinkingControlKinds,
} from '#lib/thinking-control.js'
import type { ThinkingLevelMapping } from '#lib/types.js'

const STANDARD_PROTOCOLS = [
  'open-responses',
  'openai-compatible',
  'anthropic-messages',
  'google-gemini',
  'bedrock-converse',
  'cohere-chat',
  'gateway-language-model',
]

describe('thinking control representability', () => {
  test('hidden is writable everywhere', () => {
    for (const context of [
      {},
      { protocol: 'watsonx-text-chat' },
      { protocol: 'kiro-generate-assistant-response-oauth', reasoningEfforts: [] },
      { protocol: 'openai-compatible', reasoningEfforts: ['high'] },
    ]) {
      expect(thinkingControlWritable({ type: 'hidden' }, context)).toBe(true)
      expect(writableThinkingControlKinds(context)).toContain('hidden')
    }
  })

  test('undeclared models write any non-empty effort on the seven standard protocols', () => {
    for (const protocol of STANDARD_PROTOCOLS) {
      const context = { protocol }
      expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
      expect(thinkingControlWritable({ type: 'effort', value: 'custom-level' }, context)).toBe(true)
      expect(thinkingControlWritable({ type: 'effort', value: '  ' }, context)).toBe(false)
    }
  })

  test('undeclared models on other protocols can only disable levels', () => {
    for (const protocol of [
      'watsonx-text-chat',
      'command-code',
      'devin-connect',
      'kiro-generate-assistant-response-oauth',
      null,
    ]) {
      const context = { protocol }
      expect(writableThinkingControlKinds(context)).toEqual(['hidden'])
      expect(thinkingControlWritable({ type: 'effort', value: 'high' }, context)).toBe(false)
    }
  })

  test('declared efforts are a strict allowlist even on openai-compatible', () => {
    const context = { protocol: 'openai-compatible', reasoningEfforts: ['none', 'high'] }

    expect(thinkingControlWritable({ type: 'effort', value: 'HIGH' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'effort', value: ' none ' }, context)).toBe(true)
    expect(thinkingControlWritable({ type: 'effort', value: 'medium' }, context)).toBe(false)
    expect(thinkingControlWritable({ type: 'effort', value: 'off' }, context)).toBe(false)
    expect(thinkingControlWritable({ type: 'effort', value: '' }, context)).toBe(false)
  })

  test('a declared list with no usable effort leaves only hidden, even on standard protocols', () => {
    for (const reasoningEfforts of [[], [''], ['  ']]) {
      const context = { protocol: 'anthropic-messages', reasoningEfforts }
      expect(writableThinkingControlKinds(context)).toEqual(['hidden'])
      expect(thinkingControlWritable({ type: 'effort', value: 'high' }, context)).toBe(false)
    }
  })

  test('Kiro Builder ID accepts the declared Sonnet 5.5 efforts', () => {
    const levels = ['low', 'medium', 'high', 'xhigh', 'max'] as const
    const context = { protocol: 'kiro-generate-assistant-response-oauth', reasoningEfforts: levels }
    const rows: ThinkingLevelMapping[] = levels.map((level) => ({
      level,
      control: { type: 'effort', value: level },
      source: 'generated',
    }))

    expect(unrepresentableThinkingLevels(rows, context)).toEqual([])
    expect(writableThinkingControlKinds(context)).toEqual(['effort', 'hidden'])
    for (const value of ['none', 'minimal']) {
      expect(thinkingControlWritable({ type: 'effort', value }, context)).toBe(false)
    }
    expect(unrepresentableThinkingLevels(rows, { ...context, reasoningEfforts: null })).toEqual([...levels])
  })

  test('a migrated row outside the declared efforts is reported until it is changed', () => {
    const context = { protocol: 'openai-compatible', reasoningEfforts: ['none', 'high'] }
    const rows: ThinkingLevelMapping[] = [
      { level: 'off', control: { type: 'effort', value: 'none' }, source: 'generated' },
      { level: 'medium', control: { type: 'effort', value: 'medium' }, source: 'overridden' },
      { level: 'max', control: { type: 'hidden' }, source: 'generated' },
    ]

    expect(unrepresentableThinkingLevels(rows, context)).toEqual(['medium'])
  })
})

describe('effort candidates', () => {
  test('undeclared models have no candidate list and default to the level name', () => {
    const context = { protocol: 'openai-compatible' }

    expect(effortCandidates(context)).toBeNull()
    expect(defaultEffortControl('off', context)).toEqual({ type: 'effort', value: 'none' })
    expect(defaultEffortControl('xhigh', context)).toEqual({ type: 'effort', value: 'xhigh' })
  })

  test('declared models offer usable, unique efforts in declaration order', () => {
    const context = { protocol: 'kiro', reasoningEfforts: ['none', '', 'high', 'high', ' '] }

    expect(effortCandidates(context)).toEqual(['none', 'high'])
  })

  test('switching to effort prefers the matching declared effort, else the first one', () => {
    const context = { protocol: 'openai-compatible', reasoningEfforts: ['none', 'high'] }

    expect(defaultEffortControl('off', context)).toEqual({ type: 'effort', value: 'none' })
    expect(defaultEffortControl('high', context)).toEqual({ type: 'effort', value: 'high' })
    expect(defaultEffortControl('medium', context)).toEqual({ type: 'effort', value: 'none' })

    const uppercase = { protocol: 'openai-compatible', reasoningEfforts: ['NONE', 'HIGH'] }
    expect(defaultEffortControl('off', uppercase)).toEqual({ type: 'effort', value: 'NONE' })
    expect(defaultEffortControl('high', uppercase)).toEqual({ type: 'effort', value: 'HIGH' })
  })

  test('a declared list without usable efforts falls back to the level name', () => {
    expect(defaultEffortControl('low', { reasoningEfforts: [] })).toEqual({ type: 'effort', value: 'low' })
  })
})
