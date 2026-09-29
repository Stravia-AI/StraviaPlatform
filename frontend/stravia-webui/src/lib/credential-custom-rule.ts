import * as m from '$lib/paraglide/messages.js'
import type { CustomCredentialRule, CustomCredentialRuleInput } from '$lib/types'

export type CustomRuleMode = 'simple' | 'pattern'
export type CustomRuleField = 'name' | 'description' | 'text' | 'regex' | 'secret_group' | 'keywords' | 'min_entropy'

// 两种模式的输入同时保留，切换模式不会丢失已填内容；提交时只取当前模式。
export interface CustomRuleDraft {
  name: string
  description: string
  enabled: boolean
  mode: CustomRuleMode
  text: string
  regex: string
  secretGroup: string
  keywords: string
  minEntropy: string
}

export interface CustomRuleFieldError {
  field: CustomRuleField
  message: string
}

export function emptyDraft(): CustomRuleDraft {
  return {
    name: '',
    description: '',
    enabled: true,
    mode: 'simple',
    text: '',
    regex: '',
    secretGroup: '0',
    keywords: '',
    minEntropy: '',
  }
}

export function draftFromRule(rule: CustomCredentialRule): CustomRuleDraft {
  const draft: CustomRuleDraft = {
    ...emptyDraft(),
    name: rule.name,
    description: rule.description,
    enabled: rule.enabled,
    mode: rule.spec.mode,
  }
  if (rule.spec.mode === 'simple') {
    draft.text = rule.spec.text
  } else {
    draft.regex = rule.spec.regex
    draft.secretGroup = String(rule.spec.secret_group)
    draft.keywords = rule.spec.keywords.join(', ')
    draft.minEntropy = rule.spec.min_entropy === null ? '' : String(rule.spec.min_entropy)
  }
  return draft
}

export function splitKeywords(value: string): string[] {
  return value
    .split(/[,\n]/)
    .map((keyword) => keyword.trim())
    .filter(Boolean)
}

/** 只解析数字字段；名称、文本、表达式等业务校验由后端裁决。 */
export function draftToInput(draft: CustomRuleDraft): CustomCredentialRuleInput | CustomRuleFieldError {
  const base = { name: draft.name, description: draft.description, enabled: draft.enabled }
  if (draft.mode === 'simple') return { ...base, spec: { mode: 'simple', text: draft.text } }
  const group = draft.secretGroup.trim() === '' ? 0 : Number(draft.secretGroup)
  if (!Number.isInteger(group) || group < 0) {
    return { field: 'secret_group', message: m.credential_custom_error_group_range() }
  }
  const entropyText = draft.minEntropy.trim()
  const entropy = entropyText === '' ? null : Number(entropyText)
  if (entropy !== null && !Number.isFinite(entropy)) {
    return { field: 'min_entropy', message: m.credential_custom_error_entropy_range() }
  }
  return {
    ...base,
    spec: {
      mode: 'pattern',
      regex: draft.regex,
      secret_group: group,
      keywords: splitKeywords(draft.keywords),
      min_entropy: entropy,
    },
  }
}

export function isFieldError(value: CustomCredentialRuleInput | CustomRuleFieldError): value is CustomRuleFieldError {
  return 'field' in value
}

function reasonMessage(field: string, reason: string): string | null {
  switch (`${field}:${reason}`) {
    case 'name:required':
      return m.credential_custom_error_name_required()
    case 'text:required':
      return m.credential_custom_error_text_required()
    case 'regex:required':
      return m.credential_custom_error_regex_required()
    case 'regex:invalid':
      return m.credential_custom_error_regex_invalid()
    case 'secret_group:out_of_range':
      return m.credential_custom_error_group_range()
    case 'min_entropy:out_of_range':
      return m.credential_custom_error_entropy_range()
    case 'keywords:too_many':
      return m.credential_custom_error_keywords_too_many()
    default:
      return reason === 'too_long' ? m.credential_custom_error_too_long() : null
  }
}

const fields: readonly string[] = ['name', 'description', 'text', 'regex', 'secret_group', 'keywords', 'min_entropy']

/** 把后端的字段级校验失败转为可挂在对应输入下方的本地化消息；其他错误返回 null。 */
export function customRuleFieldError(error: unknown): CustomRuleFieldError | null {
  const { code, params } = (error ?? {}) as { code?: unknown; params?: { field?: unknown; reason?: unknown } }
  if (code !== 'custom_credential_rule_invalid') return null
  const field = params?.field
  const reason = params?.reason
  if (typeof field !== 'string' || typeof reason !== 'string' || !fields.includes(field)) return null
  const message = reasonMessage(field, reason)
  return message ? { field: field as CustomRuleField, message } : null
}
