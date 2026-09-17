import { resolveProtocol } from '$lib/protocol'
import type { Provider } from '$lib/types/provider'
import type { TargetThinkingControl, ThinkingLevel, ThinkingLevelMapping } from '$lib/types/route'

export type ThinkingControlKind = TargetThinkingControl['type']

export interface ThinkingControlContext {
  protocol?: string | null
  presetKey?: string | null
  vendor?: string | null
  model?: string | null
}

const CONTROL_KINDS: ThinkingControlKind[] = ['effort', 'budget', 'enabled', 'disabled', 'hidden']

export function thinkingControlContext(provider: Provider | undefined, model: string): ThinkingControlContext {
  return {
    protocol: provider?.protocol,
    presetKey: provider?.preset_key,
    vendor: provider?.vendor,
    model,
  }
}

export function writableThinkingControlKinds(context: ThinkingControlContext): ThinkingControlKind[] {
  return CONTROL_KINDS.filter((kind) => thinkingControlKindWritable(kind, context))
}

export function thinkingControlWritable(
  control: TargetThinkingControl,
  context: ThinkingControlContext,
): boolean {
  return thinkingControlKindWritable(control.type, context)
}

export function unrepresentableThinkingLevels(
  mappings: ThinkingLevelMapping[],
  context: ThinkingControlContext,
): ThinkingLevel[] {
  return mappings
    .filter((row) => !thinkingControlWritable(row.control, context))
    .map((row) => row.level)
}

export function thinkingControlKindWritable(
  kind: ThinkingControlKind,
  context: ThinkingControlContext,
): boolean {
  if (kind === 'hidden') return true
  const protocol = resolveProtocol(context.protocol)
  switch (protocol) {
    case 'open-responses':
      return kind === 'effort' || kind === 'disabled'
    case 'anthropic-messages':
    case 'google-gemini':
      return true
    case 'openai-compatible':
      if (kind === 'effort') return true
      if (kind === 'enabled' || kind === 'disabled') return openaiCompatibleSupportsToggle(context)
      return false
    case 'command-code':
      return kind === 'effort'
    default:
      return false
  }
}

/** 与 `openai_compatible_thinking::supports_toggle` 保持同一份厂商/模型规则。 */
export function openaiCompatibleSupportsToggle(context: ThinkingControlContext): boolean {
  const model = context.model ?? ''
  const vendor = context.presetKey || context.vendor || undefined
  if (containsAsciiCaseInsensitive(model, 'minimax-m3')) return true
  if (vendorIs(vendor, ['baseten'])) return true
  if (
    vendorIs(vendor, ['opencode', 'opencode-go']) &&
    (containsAsciiCaseInsensitive(model, 'kimi-k2-thinking') || containsAsciiCaseInsensitive(model, 'glm-4.6'))
  ) {
    return true
  }
  if (
    vendorIs(vendor, ['deepseek', 'xiaomi', 'xiaomi-token-plan-sgp', 'xiaomi-token-plan-cn', 'xiaomi-token-plan-ams'])
  ) {
    return true
  }
  if (vendorIs(vendor, ['zai', 'zai-coding-plan', 'zhipuai', 'zhipuai-coding-plan'])) return true
  return vendorIs(vendor, [
    'alibaba',
    'alibaba-cn',
    'alibaba-coding-plan',
    'alibaba-coding-plan-cn',
    'alibaba-token-plan',
    'alibaba-token-plan-cn',
  ])
}

function vendorIs(vendor: string | undefined, candidates: string[]): boolean {
  return Boolean(
    vendor && candidates.some((candidate) => vendor.length === candidate.length && vendor.toLowerCase() === candidate.toLowerCase()),
  )
}

function containsAsciiCaseInsensitive(value: string, needle: string): boolean {
  return value.toLowerCase().includes(needle.toLowerCase())
}
