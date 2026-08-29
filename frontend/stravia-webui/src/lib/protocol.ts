/**
 * Protocol utilities — mirrors the backend three-layer identity model.
 *
 * Three orthogonal concepts:
 *   Protocol  — suite / wire-format family  (e.g. "openai-compatible")
 *   Endpoint  — specific API path           (e.g. "chat-completions")
 *   Vendor    — provider organisation       (e.g. "openai")
 *
 * UI only surfaces the Protocol display name; endpoints and versions are
 * internal implementation details not shown to users.
 *
 * Keep the alias table in sync with the Rust side:
 *   backend/crates/stravia-core/src/protocol/registry.rs::default_protocol_aliases
 */

// ── Protocol enum (canonical identifiers) ──────────────────────────────────

export type Protocol =
  | 'openai-compatible'
  | 'open-responses'
  | 'anthropic-messages'
  | 'google-gemini'
  | 'bedrock-converse'
  | 'cohere-chat'
  | 'watsonx-text-chat'
  | 'gateway-language-model'

export interface ProtocolMeta {
  id: Protocol
  /** Human-readable display name shown in the UI. */
  displayName: string
  /** Default base URL shown as placeholder in the provider form. */
  defaultBaseUrl: string
}

export const PROTOCOL_TABLE: ProtocolMeta[] = [
  { id: 'openai-compatible', displayName: 'OpenAI Compatible', defaultBaseUrl: 'https://api.openai.com/v1' },
  { id: 'open-responses', displayName: 'Open Responses 2026-04-24', defaultBaseUrl: 'https://api.openai.com/v1' },
  { id: 'anthropic-messages', displayName: 'Anthropic Messages', defaultBaseUrl: 'https://api.anthropic.com' },
  { id: 'google-gemini', displayName: 'Google Gemini', defaultBaseUrl: 'https://generativelanguage.googleapis.com' },
  { id: 'bedrock-converse', displayName: 'Amazon Bedrock Converse', defaultBaseUrl: '' },
  { id: 'cohere-chat', displayName: 'Cohere Chat', defaultBaseUrl: 'https://api.cohere.com/v2' },
  { id: 'watsonx-text-chat', displayName: 'watsonx.ai Text Chat', defaultBaseUrl: 'https://us-south.ml.cloud.ibm.com' },
  { id: 'gateway-language-model', displayName: 'Vercel AI Gateway Language Model', defaultBaseUrl: 'https://ai-gateway.vercel.sh/v4/ai' },
]

// ── Alias resolution ───────────────────────────────────────────────────────

/** Maps any known string (old canonical, short alias, legacy brand) → Protocol. */
const PROTOCOL_ALIASES: Record<string, Protocol> = {
  // Canonical (new)
  'openai-compatible': 'openai-compatible',
  'open-responses': 'open-responses',
  'anthropic-messages': 'anthropic-messages',
  'google-gemini': 'google-gemini',
  'bedrock-converse': 'bedrock-converse',
  'cohere-chat': 'cohere-chat',
  'watsonx-text-chat': 'watsonx-text-chat',
  'gateway-language-model': 'gateway-language-model',

  // Short names
  openai: 'openai-compatible',
  anthropic: 'anthropic-messages',
  claude: 'anthropic-messages',
  gemini: 'google-gemini',
  google: 'google-gemini',
  bedrock: 'bedrock-converse',
  cohere: 'cohere-chat',
  watsonx: 'watsonx-text-chat',
  gateway: 'gateway-language-model',

  // Deprecated aliases (old canonical slugs)
  'openai-compat': 'openai-compatible',
  'anthropic-msgs': 'anthropic-messages',
  'google-genai': 'google-gemini',
  'google-generative-ai': 'google-gemini',

  // Old canonical endpoint strings (Tier-1 backward compat)
  'openai/chat/v1': 'openai-compatible',
  'openai/embeddings/v1': 'openai-compatible',
  'anthropic/messages/2023-06-01': 'anthropic-messages',
  'google/generate/v1beta': 'google-gemini',

  // Deprecated canonical endpoint strings
  'openai-compat/chat-completions/v1': 'openai-compatible',
  'openai-compat/embeddings/v1': 'openai-compatible',
  'anthropic-msgs/messages/2023-06-01': 'anthropic-messages',
  'google-genai/generate-content/v1beta': 'google-gemini',

  // New canonical endpoint strings
  'openai-compatible/chat-completions/v1': 'openai-compatible',
  'openai-compatible/embeddings/v1': 'openai-compatible',
  'open-responses/responses/2026-04-24': 'open-responses',
  'anthropic-messages/messages/2023-06-01': 'anthropic-messages',
  'google-gemini/generate-content/v1beta': 'google-gemini',
  'bedrock-converse/converse/v1': 'bedrock-converse',
  'cohere-chat/chat/v2': 'cohere-chat',
  'watsonx-text-chat/chat/v1': 'watsonx-text-chat',
  'gateway-language-model/language-model/v4': 'gateway-language-model',
}

/**
 * Resolve any raw protocol string to a canonical `Protocol`, or `null` if unknown.
 *
 * Responses accepts only its dated canonical identity; aliases remain for the
 * other protocol families until their independent cutovers.
 */
export function resolveProtocol(raw: string | null | undefined): Protocol | null {
  if (!raw) return null
  const key = raw.trim().toLowerCase()
  return PROTOCOL_ALIASES[key] ?? null
}

/** Return the display name for a protocol string, or `null` if unknown. */
export function protocolDisplayName(raw: string | null | undefined): string | null {
  const protocol = resolveProtocol(raw)
  if (!protocol) return null
  return PROTOCOL_TABLE.find((p) => p.id === protocol)?.displayName ?? null
}

/**
 * Legacy shim — resolves a raw string and returns just the display name.
 *
 * Returns `null` when the input is unrecognised so callers can fall back
 * to showing the raw string.
 *
 * @deprecated prefer `protocolDisplayName` for new code.
 */
export function prettyName(raw: string | null | undefined): string | null {
  return protocolDisplayName(raw)
}

// ── ProtocolEndpoint (internal, not shown in UI) ───────────────────────────

export interface ProtocolEndpoint {
  protocol: Protocol
  name: string
  version: string
}

/** Parse a canonical `protocol/name/version` string into a `ProtocolEndpoint`. */
export function parseProtocolEndpoint(raw: string | null | undefined): ProtocolEndpoint | null {
  if (!raw) return null
  const parts = raw.trim().split('/')
  if (parts.length !== 3 || parts.some((p) => !p)) return null
  const protocol = resolveProtocol(parts[0])
  if (!protocol) return null
  return { protocol, name: parts[1], version: parts[2] }
}

// ── Backward-compat shims for routes.tsx ──────────────────────────────────

/** Returns true when the raw string resolves to an OpenAI-family protocol. */
export function isOpenAiProtocol(raw: string | null | undefined): boolean {
  const p = resolveProtocol(raw)
  return p === 'openai-compatible'
}

/**
 * @deprecated — kept for legacy call-sites, use `parseProtocolEndpoint` instead.
 */
export function parseProtocolId(
  raw: string | null | undefined,
): { family: string; dialect: string; version: string } | null {
  const ep = parseProtocolEndpoint(raw)
  if (ep) return { family: ep.protocol, dialect: ep.name, version: ep.version }
  // Fallback: try to parse old `family/dialect/version` form verbatim.
  if (!raw) return null
  const parts = raw.trim().split('/')
  if (parts.length === 3 && parts.every((p) => p.length > 0)) {
    return { family: parts[0], dialect: parts[1], version: parts[2] }
  }
  return null
}
