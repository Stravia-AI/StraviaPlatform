import * as m from '#lib/paraglide/messages.js'
import { formatDuration } from '#lib/format.js'
import { getLocale, type Locale } from '#lib/paraglide/runtime.js'
import type {
  ConnectionCheckStage,
  LocalizedText,
  ProviderConnectionCheckFailed,
  ProviderConnectionCheckPassed,
  ProviderValidationIssue,
} from '#lib/types.js'

const stageMessages: Record<ConnectionCheckStage, (locale: Locale) => string> = {
  configuration: (locale) => m.providers_connection_check_stage_configuration({}, { locale }),
  transport: (locale) => m.providers_connection_check_stage_transport({}, { locale }),
  upstream: (locale) => m.providers_connection_check_stage_upstream({}, { locale }),
  response: (locale) => m.providers_connection_check_stage_response({}, { locale }),
}

const reasonMessages: Record<string, (locale: Locale) => string> = {
  invalid_configuration: (locale) => m.providers_connection_check_reason_invalid_configuration({}, { locale }),
  connection_failed: (locale) => m.providers_connection_check_reason_connection_failed({}, { locale }),
  timeout: (locale) => m.providers_connection_check_reason_timeout({}, { locale }),
  authentication_failed: (locale) => m.providers_connection_check_reason_authentication_failed({}, { locale }),
  permission_denied: (locale) => m.providers_connection_check_reason_permission_denied({}, { locale }),
  rate_limited: (locale) => m.providers_connection_check_reason_rate_limited({}, { locale }),
  upstream_error: (locale) => m.providers_connection_check_reason_upstream_error({}, { locale }),
  invalid_response: (locale) => m.providers_connection_check_reason_invalid_response({}, { locale }),
  plugin_unavailable: (locale) => m.providers_connection_check_reason_plugin_unavailable({}, { locale }),
  cancelled: (locale) => m.providers_connection_check_reason_cancelled({}, { locale }),
}

const fallbackReason: Record<ConnectionCheckStage, (locale: Locale) => string> = {
  configuration: (locale) => m.providers_connection_check_reason_invalid_configuration({}, { locale }),
  transport: (locale) => m.providers_connection_check_reason_connection_failed({}, { locale }),
  upstream: (locale) => m.providers_connection_check_reason_upstream_error({}, { locale }),
  response: (locale) => m.providers_connection_check_reason_invalid_response({}, { locale }),
}

const stageReasonCodes: Record<ConnectionCheckStage, readonly string[]> = {
  configuration: ['invalid_configuration', 'plugin_unavailable', 'cancelled'],
  transport: ['connection_failed', 'timeout', 'cancelled'],
  upstream: ['authentication_failed', 'permission_denied', 'rate_limited', 'upstream_error', 'cancelled'],
  response: ['invalid_response', 'cancelled'],
}

function localized(factory: (locale: Locale) => string): LocalizedText {
  return { 'en-US': factory('en-US'), 'zh-CN': factory('zh-CN') }
}

/** Passed toast wording never claims more than the verification scope returned by the host. */
export function providerConnectionCheckSuccessMessage(
  result: ProviderConnectionCheckPassed,
  locale: Locale = getLocale(),
): string {
  const duration = formatDuration(result.duration_ms, locale)
  if (result.verified === 'credentials') {
    return m.providers_connection_check_verified_credentials({ duration }, { locale })
  }
  if (result.verified === 'endpoint_and_credentials') {
    return m.providers_connection_check_verified_endpoint_and_credentials({ duration }, { locale })
  }
  return m.providers_connection_check_verified_endpoint({ duration }, { locale })
}

/** Configuration was locally valid but the channel cannot probe its upstream. */
export function providerConnectionCheckUnsupportedMessage(locale: Locale = getLocale()): string {
  return m.providers_connection_check_unsupported({}, { locale })
}

/** One localized sentence per stage; the raw host code is never shown to users. */
export function providerConnectionCheckFailureMessage(
  result: ProviderConnectionCheckFailed,
  locale: Locale = getLocale(),
): string {
  const stageLabel = stageMessages[result.stage] ?? stageMessages.configuration
  const stageReason = stageReasonCodes[result.stage] ?? stageReasonCodes.configuration
  const codeReason = stageReason.includes(result.code) ? reasonMessages[result.code] : undefined
  let reason =
    result.stage === 'configuration' && ((result.issues?.length ?? 0) > 0 || result.field)
      ? m.providers_connection_check_configuration_issues({}, { locale })
      : (codeReason ?? fallbackReason[result.stage] ?? fallbackReason.configuration)(locale)
  if (result.upstream_status != null) {
    reason = m.providers_connection_check_reason_with_status({ reason, status: result.upstream_status }, { locale })
  }
  return m.providers_connection_check_failed({ stage: stageLabel(locale), reason }, { locale })
}

/**
 * Configuration failures carry localizable field issues. If the host only names
 * the field, synthesize a localized issue so the existing field still shows it.
 */
export function providerConnectionCheckFieldIssues(result: ProviderConnectionCheckFailed): ProviderValidationIssue[] {
  const issues = result.issues ?? []
  if (result.stage !== 'configuration' || !result.field) return issues
  if (issues.some((issue) => issue.field === result.field)) return issues
  return [
    ...issues,
    {
      field: result.field,
      code: result.code || 'invalid_configuration',
      message:
        result.field === 'base_url'
          ? localized((locale) => m.providers_connection_check_base_url_invalid({}, { locale }))
          : localized((locale) => m.providers_connection_check_field_invalid({}, { locale })),
    },
  ]
}

/** Merge preview and check issues without duplicating the same field/code/message. */
export function mergeProviderValidationIssues(
  ...lists: Array<readonly ProviderValidationIssue[] | undefined>
): ProviderValidationIssue[] {
  const seen = new Set<string>()
  const merged: ProviderValidationIssue[] = []
  for (const issue of lists.flatMap((list) => list ?? [])) {
    const key = `${issue.field ?? ''}\u0000${issue.code}\u0000${issue.message['en-US']}`
    if (seen.has(key)) continue
    seen.add(key)
    merged.push(issue)
  }
  return merged
}

export function providerConnectionCheckBaseUrlIssues(
  issues: readonly ProviderValidationIssue[],
): ProviderValidationIssue[] {
  return issues.filter((issue) => issue.field === 'base_url')
}

export function providerConnectionCheckConfigurationIssues(
  issues: readonly ProviderValidationIssue[],
): ProviderValidationIssue[] {
  return issues.filter((issue) => issue.field !== 'base_url')
}
