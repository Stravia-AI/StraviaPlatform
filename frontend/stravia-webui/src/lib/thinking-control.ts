import { resolveProtocol, type Protocol } from '#lib/protocol.js'
import type { Provider } from '#lib/types/provider.js'
import type { TargetThinkingControl, ThinkingLevel, ThinkingLevelMapping } from '#lib/types/route.js'

export type ThinkingControlKind = TargetThinkingControl['type']

export interface ThinkingControlContext {
  protocol?: string | null
  /** `null`/`undefined` means the model declares nothing; a declared list (even empty) is a strict allowlist. */
  reasoningEfforts?: readonly string[] | null
}

/** Mirrors `Protocol::represents_target_thinking_control` for undeclared models. */
const PROTOCOL_WRITES_EFFORT: Record<Protocol, boolean> = {
  'open-responses': true,
  'openai-compatible': true,
  'anthropic-messages': true,
  'google-gemini': true,
  'bedrock-converse': true,
  'cohere-chat': true,
  'gateway-language-model': true,
  'watsonx-text-chat': false,
  'command-code': false,
  'devin-connect': false,
}

export function thinkingControlContext(
  provider: Provider | undefined,
  reasoningEfforts: ThinkingControlContext['reasoningEfforts'],
): ThinkingControlContext {
  return { protocol: provider?.protocol, reasoningEfforts }
}

export function writableThinkingControlKinds(context: ThinkingControlContext): ThinkingControlKind[] {
  const effortWritable =
    context.reasoningEfforts == null
      ? protocolWritesEffort(context)
      : context.reasoningEfforts.some((effort) => effort.trim() !== '')
  return effortWritable ? ['effort', 'hidden'] : ['hidden']
}

export function thinkingControlWritable(control: TargetThinkingControl, context: ThinkingControlContext): boolean {
  if (control.type === 'hidden') return true
  const value = control.value.trim()
  if (!value) return false
  if (context.reasoningEfforts == null) return protocolWritesEffort(context)
  // The host compares native effort identifiers with Rust's eq_ignore_ascii_case.
  const needle = asciiLowercase(value)
  return context.reasoningEfforts.some((candidate) => asciiLowercase(candidate) === needle)
}

export function unrepresentableThinkingLevels(
  mappings: ThinkingLevelMapping[],
  context: ThinkingControlContext,
): ThinkingLevel[] {
  return mappings.filter((row) => !thinkingControlWritable(row.control, context)).map((row) => row.level)
}

/** Declared efforts an `effort` row can pick from; `null` when the model declares none and any value may be typed. */
export function effortCandidates(context: ThinkingControlContext): string[] | null {
  if (context.reasoningEfforts == null) return null
  const candidates = context.reasoningEfforts.filter((effort) => effort !== '' && effort === effort.trim())
  return [...new Set(candidates)]
}

/** The control a row gets when switched to `effort`: its own level name, else the first declared candidate. */
export function defaultEffortControl(
  level: ThinkingLevel,
  context: ThinkingControlContext,
): Extract<TargetThinkingControl, { type: 'effort' }> {
  const natural = level === 'off' ? 'none' : level
  const candidates = effortCandidates(context)
  const matched = candidates?.find((candidate) => asciiLowercase(candidate) === natural)
  return { type: 'effort', value: matched ?? (candidates?.length ? candidates[0] : natural) }
}

function protocolWritesEffort(context: ThinkingControlContext): boolean {
  const protocol = resolveProtocol(context.protocol)
  return protocol !== null && PROTOCOL_WRITES_EFFORT[protocol]
}

function asciiLowercase(value: string): string {
  return value.replace(/[A-Z]/g, (char) => char.toLowerCase())
}
