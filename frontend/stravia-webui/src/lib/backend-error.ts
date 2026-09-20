import * as m from '$lib/paraglide/messages.js'
import { formatList } from '$lib/format'
import { getLocale, type Locale } from '$lib/paraglide/runtime.js'

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
  const options = { locale }
  switch (kind) {
    case 'effort':
      return m.model_editor_thinking_effort({}, options)
    case 'budget':
      return m.model_editor_thinking_budget({}, options)
    case 'enabled':
      return m.model_editor_thinking_enabled({}, options)
    case 'disabled':
      return m.model_editor_thinking_disabled({}, options)
    case 'hidden':
      return m.model_editor_thinking_hidden({}, options)
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

export function localizeBackendErrorMessage(error: unknown, locale: Locale = getLocale()): string {
  const { raw, payload } = resolveErrorPayload(error)
  const options = { locale }
  if (!payload?.code) return m.backend_error_unknown({ message: raw }, options)

  const name = extractName(payload.params) || m.backend_error_unnamed({}, options)
  switch (payload.code) {
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
    case 'credential_test_failed':
      return m.credential_protection_tester_failed({}, options)
    case 'PROVIDER_NAME_CONFLICT':
      return m.backend_error_provider_name_conflict({ name }, options)
    case 'ROUTE_ID_CONFLICT':
      return m.backend_error_route_id_conflict({ route_id: extractRouteId(payload.params) }, options)
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
      const controls = extractNamedStrings(payload.params, 'controls', 'control')
      const supported = extractStringArray(payload.params, 'supported_controls')
      return m.backend_error_thinking_control_unrepresentable(
        {
          provider_id: extractString(payload.params, 'provider_id'),
          model_id: extractString(payload.params, 'model_id'),
          levels: formatList(levels, locale) || extractString(payload.params, 'level'),
          controls:
            formatList(
              controls.map((kind) => thinkingControlKindLabel(kind, locale)),
              locale,
            ) || (locale === 'zh-CN' ? '当前控制' : 'the current control'),
          supported: formatList(
            (supported.length > 0 ? supported : ['effort', 'hidden']).map((kind) =>
              thinkingControlKindLabel(kind, locale),
            ),
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
    case 'CATALOG_ENTRY_NOT_FOUND':
      return m.backend_error_catalog_entry_not_found({}, options)
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
    case 'media_generation_config_invalid':
      return m.media_generation_config_invalid({}, options)
    case 'media_generation_config_unavailable':
      return m.media_generation_config_unavailable({}, options)
    case 'media_generation_route_missing':
      return m.media_generation_route_required({}, options)
    case 'media_generation_route_disabled':
      return m.media_generation_route_disabled({}, options)
    case 'media_generation_targets_missing':
      return m.media_generation_targets_missing({}, options)
    case 'media_generation_provider_missing':
      return m.media_generation_provider_missing({}, options)
    case 'media_generation_target_incompatible':
      return m.media_generation_target_incompatible({}, options)
    case 'media_generation_oauth_unavailable':
      return m.media_generation_oauth_unavailable({}, options)
    default:
      return m.backend_error_unknown({ message: payload.message || raw }, options)
  }
}
