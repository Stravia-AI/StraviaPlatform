import * as m from '#lib/paraglide/messages.js'
import { formatList } from '#lib/format.js'
import { routePurposeLabels } from '#lib/logical-model.js'
import { routePurposes } from '#lib/types/route.js'
import { getLocale, type Locale } from '#lib/paraglide/runtime.js'

type ErrorPayload = { code?: string; message?: string; params?: Record<string, unknown> }

function extractRawErrorMessage(error: unknown): string {
  if (typeof error === 'string') return error
  if (error instanceof Error && typeof error.message === 'string') return error.message
  if (error && typeof error === 'object') {
    const obj = error as Record<string, unknown>
    if (typeof obj.message === 'string') return obj.message
    if (typeof obj.error === 'string') return obj.error
  }
  return String(error)
}

function parseErrorPayload(raw: string): ErrorPayload | null {
  const text = raw.trim()
  if (!text.startsWith('{') || !text.endsWith('}')) return null
  try {
    const parsed = JSON.parse(text) as ErrorPayload
    if (!parsed || typeof parsed !== 'object') return null
    return parsed
  } catch {
    return null
  }
}

function extractName(params?: Record<string, unknown>) {
  if (!params || typeof params !== 'object') return ''
  const value = params.name
  return typeof value === 'string' ? value : ''
}

function extractRouteId(params?: Record<string, unknown>) {
  if (!params || typeof params !== 'object') return ''
  const value = params.route_id
  return typeof value === 'string' ? value : ''
}

function extractRouteCount(params?: Record<string, unknown>) {
  if (!params || typeof params !== 'object') return 0
  const value = params.routeCount
  return typeof value === 'number' && Number.isFinite(value) ? value : 0
}

function extractProtocol(params?: Record<string, unknown>) {
  if (!params || typeof params !== 'object') return ''
  const value = params.protocol
  return typeof value === 'string' ? value : ''
}

function extractModel(params?: Record<string, unknown>) {
  if (!params || typeof params !== 'object') return ''
  const value = params.model
  return typeof value === 'string' ? value : ''
}

function extractString(params: Record<string, unknown> | undefined, key: string): string {
  const value = params?.[key]
  return typeof value === 'string' ? value : ''
}

function extractStringArray(params: Record<string, unknown> | undefined, key: string): string[] {
  const value = params?.[key]
  if (!Array.isArray(value)) return []
  return value.filter((item): item is string => typeof item === 'string' && item.length > 0)
}

function extractNamedStrings(
  params: Record<string, unknown> | undefined,
  listKey: string,
  fallbackKey: string,
): string[] {
  const values = extractStringArray(params, listKey)
  if (values.length > 0) return values
  const fallback = extractString(params, fallbackKey)
  return fallback ? [fallback] : []
}

function thinkingControlKindLabel(kind: string, locale: Locale): string {
  switch (kind) {
    case 'effort':
      return m.model_editor_thinking_effort({}, { locale })
    case 'hidden':
      return m.model_editor_thinking_hidden({}, { locale })
    default:
      return kind
  }
}

function resolveErrorPayload(error: unknown): { raw: string; payload: ErrorPayload | null } {
  const raw = extractRawErrorMessage(error)
  const direct =
    error && typeof error === 'object' && typeof (error as ErrorPayload).code === 'string'
      ? (error as ErrorPayload)
      : null
  return { raw, payload: direct ?? parseErrorPayload(raw) }
}

export function isRouteBindingError(error: unknown): boolean {
  const code = resolveErrorPayload(error).payload?.code
  return code === 'ROUTE_IMAGE_GENERATION_BOUND' || code === 'ROUTE_WEB_SEARCH_BOUND'
}

export function unrepresentableThinkingTarget(error: unknown): { providerId: string; modelId: string } | null {
  const payload = resolveErrorPayload(error).payload
  if (payload?.code !== 'THINKING_CONTROL_UNREPRESENTABLE') return null
  const providerId = extractString(payload.params, 'provider_id')
  const modelId = extractString(payload.params, 'model_id')
  if (!providerId || !modelId) return null
  return { providerId, modelId }
}

// 目录选择过期:服务/渠道被新 revision 移除,或渠道指纹变更。
// 这类失败重试不可能成功,调用方应刷新服务列表并让用户重新选择。
export function catalogSelectionStale(error: unknown): boolean {
  const code = resolveErrorPayload(error).payload?.code
  return (
    code === 'CATALOG_PROVIDER_NOT_FOUND' ||
    code === 'CATALOG_CHANNEL_NOT_FOUND' ||
    code === 'CATALOG_FINGERPRINT_STALE'
  )
}

export function localizeMarketplaceError(error: unknown, locale: Locale = getLocale()): string {
  const options = { locale }
  switch (resolveErrorPayload(error).payload?.code) {
    case 'not_cataloged':
      return m.plugin_sources_error_not_cataloged({}, options)
    case 'key_changed':
      return m.plugin_sources_error_key_changed({}, options)
    case 'rate_limited':
      return m.plugin_sources_error_rate_limited({}, options)
    case 'network':
      return m.plugin_sources_error_network({}, options)
    case 'signature_invalid':
      return m.plugin_sources_error_signature_invalid({}, options)
    case 'source_identity_changed':
      return m.plugin_sources_error_identity({}, options)
    case 'version_conflict':
      return m.market_error_version_conflict({}, options)
    case 'incompatible':
      return m.market_error_incompatible({}, options)
    case 'artifact_corrupt':
      return m.market_error_artifact_corrupt({}, options)
    case 'preview_expired':
      return m.market_error_preview_expired({}, options)
    case 'source_disabled':
      return m.market_error_source_disabled({}, options)
    case 'resource_limit':
      return m.market_error_resource_limit({}, options)
    default:
      return localizeBackendErrorMessage(error, locale)
  }
}

export function localizeBackendErrorMessage(error: unknown, locale: Locale = getLocale()): string {
  const { raw, payload } = resolveErrorPayload(error)
  const options = { locale }
  if (!payload?.code) return m.backend_error_unknown({ message: raw }, options)

  const name = extractName(payload.params) || m.backend_error_unnamed({}, options)
  switch (payload.code) {
    case 'VENDOR_PLUGIN_INCOMPATIBLE_ABI':
      return m.backend_error_vendor_plugin_incompatible({}, options)
    case 'VENDOR_PLUGIN_REQUIRES_NEWER_HOST':
      return m.backend_error_vendor_plugin_requires_newer_host({}, options)
    case 'invalid_credentials':
      return m.login_invalid_credentials({}, options)
    case 'unauthorized':
      return m.frontend_error_authentication_required({}, options)
    case 'auth_unavailable':
      return m.admin_auth_service_unavailable({}, options)
    case 'invalid_input':
      return m.admin_auth_invalid_input({}, options)
    case 'origin_required':
    case 'origin_mismatch':
    case 'csrf_required':
    case 'json_required':
      return m.admin_auth_request_rejected({}, options)
    case 'invalid_setup_token':
      return m.setup_error_token({}, options)
    case 'setup_unauthorized':
      return m.setup_error_session({}, options)
    case 'setup_complete':
    case 'conflict':
      return m.setup_error_completed({}, options)
    case 'invalid_database_config':
      return m.setup_error_database_config({}, options)
    case 'database_unavailable':
      return m.setup_error_database_connection({}, options)
    case 'config_save_failed':
      return m.setup_error_config_save({}, options)
    case 'gateway_unavailable':
      return m.setup_error_gateway({}, options)
    case 'credential_rules_unavailable':
      return m.credential_protection_catalog_failed({}, options)
    case 'credential_rule_unknown':
      return m.credential_protection_rule_unknown({ rule_id: extractString(payload.params, 'rule_id') }, options)
    case 'credential_disabled_rules_unavailable':
      return m.credential_protection_rules_save_failed({}, options)
    case 'credential_test_failed':
      return m.credential_protection_tester_failed({}, options)
    case 'custom_credential_rule_invalid':
      return m.credential_custom_error_invalid({}, options)
    case 'custom_credential_rule_not_found':
      return m.credential_custom_error_not_found({}, options)
    case 'custom_credential_rule_unavailable':
      return m.credential_custom_error_unavailable({}, options)
    case 'WEB_PROVIDER_UNAVAILABLE': {
      const detail = (payload.message ?? (raw.startsWith('{') ? '' : raw)).trim()
      return detail && !/^HTTP \d+$/.test(detail)
        ? m.backend_error_web_provider_unavailable_reason({ reason: detail }, options)
        : m.backend_error_web_provider_unavailable({}, options)
    }
    case 'WEB_PROVIDER_SINGLETON_CONFLICT': {
      const kind = extractString(payload.params, 'kind')
      return kind
        ? m.backend_error_web_provider_singleton_conflict({ kind }, options)
        : m.backend_error_web_provider_singleton_conflict_generic({}, options)
    }
    case 'WEB_PROVIDER_DELETE_FORBIDDEN':
      return m.backend_error_web_provider_delete_forbidden({}, options)
    case 'WEB_PROVIDER_NAME_CONFLICT':
      return m.backend_error_web_provider_name_conflict({ name }, options)
    case 'PROVIDER_MODEL_INVALID_CHANGES': {
      const detail = extractString(payload.params, 'error')
      return m.backend_error_unknown({ message: detail || payload.message || raw }, options)
    }
    case 'PROVIDER_NAME_CONFLICT':
      return m.backend_error_provider_name_conflict({ name }, options)
    case 'ROUTE_ID_CONFLICT':
      return m.backend_error_route_id_conflict({ route_id: extractRouteId(payload.params) }, options)
    case 'ROUTE_NOT_FOUND': {
      const message = m.backend_error_route_create_before_binding({}, options)
      const route_id = extractRouteId(payload.params)
      return route_id ? m.backend_error_route_context({ route_id, message }, options) : message
    }
    case 'ROUTE_PURPOSE_UNCONFIGURED':
    case 'route_purpose_unconfigured': {
      const message = m.backend_error_route_purpose_unconfigured({}, options)
      const route_id = extractRouteId(payload.params)
      return route_id ? m.backend_error_route_context({ route_id, message }, options) : message
    }
    case 'ROUTE_PURPOSE_UNREGISTERED': {
      const provider_id = extractString(payload.params, 'provider_id')
      const model_id = extractString(payload.params, 'model_id')
      return provider_id && model_id
        ? m.backend_error_route_purpose_unregistered_target({ provider_id, model_id }, options)
        : m.backend_error_route_purpose_unregistered({}, options)
    }
    case 'ROUTE_PURPOSE_MISMATCH': {
      const provider_id = extractString(payload.params, 'provider_id')
      const model_id = extractString(payload.params, 'model_id')
      const requested = extractString(payload.params, 'purpose')
      const known = routePurposes.find((value) => value === requested)
      const purpose = known ? routePurposeLabels[known]({}, options) : requested
      return provider_id && model_id && purpose
        ? m.backend_error_route_purpose_mismatch_target({ provider_id, model_id, purpose }, options)
        : m.backend_error_route_purpose_mismatch({}, options)
    }
    case 'target_purpose_mismatch':
      return m.backend_error_route_purpose_mismatch({}, options)
    case 'ROUTE_IMAGE_GENERATION_BOUND':
    case 'ROUTE_WEB_SEARCH_BOUND': {
      const message =
        payload.code === 'ROUTE_IMAGE_GENERATION_BOUND'
          ? m.backend_error_route_image_generation_bound({}, options)
          : m.backend_error_route_web_search_bound({}, options)
      const route_id = extractRouteId(payload.params)
      return route_id ? m.backend_error_route_context({ route_id, message }, options) : message
    }
    case 'WEB_SEARCH_MODEL_INELIGIBLE':
      return m.web_search_local_model_unavailable({}, options)
    case 'WEB_SEARCH_ROUTE_UNAVAILABLE':
      return m.web_search_external_route_unavailable({}, options)
    case 'route_purpose_mismatch':
      return m.backend_error_request_route_purpose_mismatch({}, options)
    case 'route_purpose_unavailable':
      return m.backend_error_route_purpose_unavailable({}, options)
    case 'API_KEY_NAME_CONFLICT':
      return m.backend_error_api_key_name_conflict({ name }, options)
    case 'PROVIDER_IN_USE': {
      const routeCount = extractRouteCount(payload.params)
      return routeCount > 0
        ? m.backend_error_provider_in_use_routes({ count: routeCount }, options)
        : m.backend_error_provider_in_use_generic({}, options)
    }
    case 'ROUTE_PROTOCOL_MODEL_CONFLICT':
      return m.backend_error_protocol_model_conflict(
        { protocol: extractProtocol(payload.params) || 'openai', model: extractModel(payload.params) || '(unknown)' },
        options,
      )
    case 'THINKING_LEVEL_COVERAGE_REQUIRED':
      return m.backend_error_thinking_level_coverage(
        {
          provider_id: extractString(payload.params, 'provider_id'),
          model_id: extractString(payload.params, 'model_id'),
          level: extractString(payload.params, 'level'),
        },
        options,
      )
    case 'THINKING_CONTROL_UNREPRESENTABLE': {
      const levels = extractNamedStrings(payload.params, 'levels', 'level')
      const supported = extractStringArray(payload.params, 'supported_controls')
      return m.backend_error_thinking_control_unrepresentable(
        {
          provider_id: extractString(payload.params, 'provider_id'),
          model_id: extractString(payload.params, 'model_id'),
          levels: formatList(levels, locale),
          supported: formatList(
            (supported.length ? supported : ['effort', 'hidden']).map((kind) => thinkingControlKindLabel(kind, locale)),
            locale,
          ),
        },
        options,
      )
    }
    case 'CATALOG_REFRESH_FAILED':
      return m.backend_error_catalog_refresh_failed({}, options)
    case 'CATALOG_SCOPE_REFRESH_FAILED':
      return m.backend_error_catalog_scope_refresh_failed({}, options)
    case 'PROVIDER_ALLOWANCE_LOAD_FAILED':
      return m.backend_error_provider_allowance_load_failed({}, options)
    case 'PROVIDER_ALLOWANCE_REFRESH_FAILED':
      return m.backend_error_provider_allowance_refresh_failed({}, options)
    case 'PROVIDER_ALLOWANCE_UNAVAILABLE':
      return m.backend_error_provider_allowance_unavailable({}, options)
    case 'CATALOG_MODEL_NOT_FOUND':
      return m.backend_error_catalog_model_not_found({}, options)
    case 'CATALOG_PROVIDER_NOT_FOUND':
      return m.backend_error_catalog_provider_not_found({}, options)
    case 'CATALOG_CHANNEL_NOT_FOUND':
      return m.backend_error_catalog_channel_not_found({}, options)
    case 'CATALOG_FINGERPRINT_STALE':
      return m.backend_error_catalog_fingerprint_stale({}, options)
    case 'AUTH_ACCESS_DENIED':
      return m.backend_error_auth_access_denied({}, options)
    case 'AUTH_CALLBACK_URL_INVALID':
      return m.backend_error_auth_callback_url_invalid({}, options)
    case 'AUTH_CALLBACK_CODE_MISSING':
      return m.backend_error_auth_callback_code_missing({}, options)
    case 'AUTH_CALLBACK_STATE_MISSING':
    case 'AUTH_CALLBACK_STATE_MISMATCH':
      return m.backend_error_auth_callback_state_invalid({}, options)
    case 'AUTH_INVALID_GRANT':
      return m.backend_error_auth_invalid_grant({}, options)
    case 'AUTH_CONFIGURATION_ERROR':
      return m.backend_error_auth_configuration({}, options)
    case 'AUTH_TIMEOUT':
      return m.backend_error_auth_timeout({}, options)
    case 'AUTH_SESSION_REPLACED':
      return m.backend_error_auth_session_replaced({}, options)
    case 'AUTH_LISTENER_FATAL':
      return m.backend_error_auth_listener_fatal({}, options)
    case 'AUTH_SESSION_REQUIRED':
      return m.backend_error_auth_session_required({}, options)
    case 'AUTH_COMPLETION_IN_PROGRESS':
      return m.backend_error_auth_completion_in_progress({}, options)
    case 'AUTH_EXCHANGE_REJECTED':
      return m.backend_error_auth_exchange_rejected({}, options)
    case 'AUTH_EXCHANGE_RETRYABLE':
      return m.backend_error_auth_exchange_retryable({}, options)
    case 'AUTH_REFRESH_TOKEN_REQUIRED':
      return m.backend_error_auth_refresh_token_required({}, options)
    case 'AUTH_REFRESH_TOKEN_INVALID':
      return m.backend_error_auth_refresh_token_invalid({}, options)
    case 'AUTH_REFRESH_TOKEN_NOT_SUPPORTED':
      return m.backend_error_auth_refresh_token_not_supported({}, options)
    case 'media_generation_config_invalid':
      return m.media_generation_config_invalid({}, options)
    case 'media_generation_config_unavailable':
      return m.media_generation_config_unavailable({}, options)
    case 'media_generation_route_missing':
      return m.media_generation_route_required({}, options)
    case 'media_generation_route_disabled':
      return m.media_generation_route_disabled({}, options)
    case 'media_generation_route_purpose_mismatch':
      return m.media_generation_route_purpose_mismatch({}, options)
    case 'media_generation_targets_missing':
      return m.media_generation_targets_missing({}, options)
    case 'media_generation_targets_purpose_mismatch':
      return m.media_generation_targets_purpose_mismatch({}, options)
    case 'media_generation_provider_missing':
      return m.media_generation_provider_missing({}, options)
    case 'media_generation_target_incompatible':
      return m.media_generation_target_incompatible({}, options)
    default:
      return m.backend_error_unknown({ message: payload.message || raw }, options)
  }
}
