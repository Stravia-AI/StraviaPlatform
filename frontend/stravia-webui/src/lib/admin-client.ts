import * as m from '$lib/paraglide/messages.js'

import { clearAdminToken, getAdminToken } from '$lib/auth'
import type { ConnectClientApplyPlan } from '$lib/connect-client-apply'
import type { ConnectClientApplyRequest } from '$lib/connect'
import type { Locale } from '$lib/paraglide/runtime.js'
import type {
  ApiKey,
  ProviderModelDetail,
  PreparedProviderModel,
  ProviderModelList,
  ProviderModelSelectionPolicy,
  ProviderModelSyncSummary,
  CatalogProviderList,
  CanonicalModelList,
  CatalogRefreshSummary,
  ApiKeyStats,
  CreateApiKey,
  BindRouteInput,
  CreateRoute,
  CreateProvider,
  CreateWebProvider,
  GatewayStatus,
  LogPage,
  LogQuery,
  Route,
  ImageCapabilityDrift,
  ModelCapabilities,
  ModelStats,
  OAuthCallbackMode,
  OAuthSessionInitData,
  OAuthSessionStatusData,
  Provider,
  ProviderOAuthStatusData,
  ProviderStats,
  StatsHourly,
  StatsOverview,
  TestResult,
  UpdateApiKey,
  UnbindRouteInput,
  UpdateRoute,
  UpdateProvider,
  UpdateWebProvider,
  VendorMetadata,
  WebAccessSettings,
  WebProvider,
  WebSearchConfigView,
  UpdateWebSearchConfig,
  EligibleSearchModel,
  CompatibleCodexProvider,
  MediaUnderstandingConfigView,
  UpdateMediaUnderstandingConfig,
  ThinkingLevel,
  ProviderAllowanceSnapshot,
} from '$lib/types'

export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

let desktopApiBase: Promise<string> | undefined

async function resolveApiBase(): Promise<string> {
  if (!isTauri) return '/api/v1'

  desktopApiBase ??= import('@tauri-apps/api/core')
    .then(({ invoke }) => invoke<number>('get_server_port'))
    .then((port) => `http://127.0.0.1:${port}/api/v1`)
  return desktopApiBase
}

type HttpMethod = 'DELETE' | 'GET' | 'POST' | 'PUT'

interface RequestMapping {
  method: HttpMethod
  path: string
  body?: Record<string, unknown> | string
}

interface ResponsePayload {
  data?: unknown
  error?: unknown
  code?: string
  params?: Record<string, unknown>
}

function mapRequest(command: string, args?: Record<string, unknown>): RequestMapping {
  switch (command) {
    case 'listProviders':
      return { method: 'GET', path: '/providers' }
    case 'previewConnectClient':
      return {
        method: 'POST',
        path: '/connect-clients/preview',
        body: args?.input as unknown as Record<string, unknown>,
      }
    case 'listCatalogProviders':
      return { method: 'GET', path: '/catalog/providers' }
    case 'listVendorMetadata':
      return { method: 'GET', path: '/vendors' }
    case 'listCanonicalModels':
      return { method: 'GET', path: '/catalog/models' }
    case 'refreshCatalog':
      return { method: 'POST', path: '/catalog/refresh' }
    case 'createProvider':
      return { method: 'POST', path: '/providers', body: args?.input as Record<string, unknown> }
    case 'previewProviderBaseUrl':
      return {
        method: 'POST',
        path: '/providers/base-url/preview',
        body: { vendor_id: args?.vendorId, adapter_credentials: args?.adapterCredentials, base_url: args?.baseUrl },
      }
    case 'copyProvider':
      return {
        method: 'POST',
        path: `/providers/${args?.id}/copy`,
        body: (args?.options ?? {}) as Record<string, unknown>,
      }
    case 'updateProvider':
      return { method: 'PUT', path: `/providers/${args?.id}`, body: args?.input as Record<string, unknown> }
    case 'deleteProvider':
      return { method: 'DELETE', path: `/providers/${args?.id}` }
    case 'testProvider':
      return { method: 'GET', path: `/providers/${args?.id}/test` }
    case 'listWebProviders':
      return { method: 'GET', path: '/web-providers' }
    case 'createWebProvider':
      return { method: 'POST', path: '/web-providers', body: args?.input as Record<string, unknown> }
    case 'updateWebProvider':
      return { method: 'PUT', path: `/web-providers/${args?.id}`, body: args?.input as Record<string, unknown> }
    case 'deleteWebProvider':
      return { method: 'DELETE', path: `/web-providers/${args?.id}` }
    case 'testWebProvider':
      return { method: 'POST', path: `/web-providers/${args?.id}/test` }
    case 'getWebAccessSettings':
      return { method: 'GET', path: '/web-access/settings' }
    case 'updateWebAccessSettings':
      return { method: 'PUT', path: '/web-access/settings', body: args?.input as Record<string, unknown> }
    case 'getWebSearchConfig':
      return { method: 'GET', path: '/web-search/config' }
    case 'updateWebSearchConfig':
      return { method: 'PUT', path: '/web-search/config', body: args?.input as Record<string, unknown> }
    case 'listEligibleWebSearchModels':
      return { method: 'GET', path: '/web-search/eligible-models' }
    case 'listCompatibleCodexSearchProviders':
      return { method: 'GET', path: '/web-search/codex-providers' }
    case 'getMediaUnderstandingConfig':
      return { method: 'GET', path: '/media-understanding' }
    case 'updateMediaUnderstandingConfig':
      return { method: 'PUT', path: '/media-understanding', body: args?.input as Record<string, unknown> }
    case 'testProviderModels':
      return { method: 'GET', path: `/providers/${args?.id}/test-models` }
    case 'listImageCapabilityDrifts':
      return { method: 'GET', path: '/providers/image-capability-drifts' }
    case 'listProviderModels':
      return { method: 'GET', path: `/providers/${args?.id}/models` }
    case 'syncProviderModels':
      return { method: 'POST', path: `/providers/${args?.id}/models/sync` }
    case 'prepareProviderModel':
      return {
        method: 'POST',
        path: `/providers/${args?.id}/model/prepare`,
        body: { model_id: args?.modelId, template_id: args?.templateId },
      }
    case 'getProviderModel':
      return {
        method: 'GET',
        path: `/providers/${args?.id}/model?model=${encodeURIComponent(String(args?.modelId ?? ''))}`,
      }
    case 'createManualProviderModel':
      return {
        method: 'POST',
        path: `/providers/${args?.id}/models`,
        body: `{"model_id":${JSON.stringify(String(args?.modelId ?? ''))},"metadata":${String(args?.metadataJson)}}`,
      }
    case 'updateProviderModel':
      return {
        method: 'PUT',
        path: `/providers/${args?.id}/model`,
        body: `{"model_id":${JSON.stringify(String(args?.modelId ?? ''))},"metadata":${String(args?.metadataJson)},"revision":${Number(args?.revision)}}`,
      }
    case 'updateProviderModelSelection':
      return {
        method: 'PUT',
        path: `/providers/${args?.id}/model/selection`,
        body: { model_id: args?.modelId, policy: args?.policy, revision: args?.revision },
      }
    case 'reimportProviderModel':
      return {
        method: 'POST',
        path: `/providers/${args?.id}/model/reimport`,
        body: { model_id: args?.modelId, revision: args?.revision },
      }
    case 'deleteManualProviderModel':
      return {
        method: 'DELETE',
        path: `/providers/${args?.id}/model?model=${encodeURIComponent(String(args?.modelId ?? ''))}`,
      }
    case 'getModelCapabilities':
      return {
        method: 'GET',
        path: `/providers/${args?.providerId}/model-capabilities?model=${encodeURIComponent(String(args?.model ?? ''))}`,
      }
    case 'getProviderOAuthStatus':
      return { method: 'GET', path: `/providers/${args?.id}/oauth/status` }
    case 'reconnectProviderOAuth':
      return { method: 'POST', path: `/providers/${args?.id}/oauth/reconnect` }
    case 'logoutProviderOAuth':
      return { method: 'POST', path: `/providers/${args?.id}/oauth/logout` }
    case 'bindProviderOAuth':
      return {
        method: 'POST',
        path: `/providers/${args?.providerId ?? args?.id}/oauth/bind`,
        body: { session_id: args?.sessionId },
      }
    case 'initOAuthSession':
      return {
        method: 'POST',
        path: '/oauth/sessions/init',
        body: {
          vendor: args?.vendor,
          use_proxy: args?.useProxy,
          callback_mode: args?.callbackMode,
          locale: args?.locale,
        },
      }
    case 'getOAuthSessionStatus':
      return { method: 'GET', path: `/oauth/sessions/${args?.sessionId}/status` }
    case 'cancelOAuthSession':
      return { method: 'POST', path: `/oauth/sessions/${args?.sessionId}/cancel` }
    case 'updateOAuthSessionProxy':
      return { method: 'PUT', path: `/oauth/sessions/${args?.sessionId}/proxy`, body: { use_proxy: args?.useProxy } }
    case 'completeOAuthSession':
      return {
        method: 'POST',
        path: `/oauth/sessions/${args?.sessionId}/complete`,
        body: { callback_url: args?.callbackUrl, metadata: args?.metadata ?? {} },
      }
    case 'createOAuthProvider':
      return { method: 'POST', path: '/providers/oauth', body: { session_id: args?.sessionId, input: args?.input } }
    case 'listModels':
      return { method: 'GET', path: '/models' }
    case 'getModel':
      return { method: 'GET', path: `/models/${encodeURIComponent(String(args?.routeId ?? ''))}` }
    case 'createModel':
      return { method: 'POST', path: '/models', body: args?.input as Record<string, unknown> }
    case 'bindRoute':
      return { method: 'POST', path: '/models/bind', body: args?.input as Record<string, unknown> }
    case 'unbindRoute':
      return { method: 'POST', path: '/models/unbind', body: args?.input as Record<string, unknown> }
    case 'updateModel':
      return {
        method: 'PUT',
        path: `/models/${encodeURIComponent(String(args?.routeId ?? ''))}`,
        body: args?.input as Record<string, unknown>,
      }
    case 'deleteModel':
      return { method: 'DELETE', path: `/models/${encodeURIComponent(String(args?.routeId ?? ''))}` }
    case 'resetTargetThinkingMapping':
      return {
        method: 'POST',
        path: `/models/${encodeURIComponent(String(args?.routeId ?? ''))}/targets/${args?.targetId}/thinking-map/reset`,
        body: { level: args?.level },
      }
    case 'regenerateTargetThinkingMap':
      return {
        method: 'POST',
        path: `/models/${encodeURIComponent(String(args?.routeId ?? ''))}/targets/${args?.targetId}/thinking-map/regenerate`,
      }
    case 'listApiKeys':
      return { method: 'GET', path: '/api-keys' }
    case 'createApiKey':
      return { method: 'POST', path: '/api-keys', body: args?.input as Record<string, unknown> }
    case 'updateApiKey':
      return { method: 'PUT', path: `/api-keys/${args?.id}`, body: args?.input as Record<string, unknown> }
    case 'deleteApiKey':
      return { method: 'DELETE', path: `/api-keys/${args?.id}` }
    case 'queryLogs': {
      const query = args?.query as LogQuery | undefined
      const params = new URLSearchParams()
      for (const [key, value] of Object.entries(query ?? {})) {
        if (value != null && value !== '') params.set(key, String(value))
      }
      const suffix = params.size > 0 ? `?${params}` : ''
      return { method: 'GET', path: `/logs${suffix}` }
    }
    case 'getLog':
      return { method: 'GET', path: `/logs/${args?.id}` }
    case 'clearLogs':
      return { method: 'DELETE', path: '/logs' }
    case 'getStatsOverview':
      return { method: 'GET', path: statsPath('/stats/overview', args?.hours) }
    case 'getStatsHourly':
      return { method: 'GET', path: statsPath('/stats/hourly', args?.hours ?? 24) }
    case 'getStatsByModel':
      return { method: 'GET', path: statsPath('/stats/models', args?.hours) }
    case 'getStatsByProvider':
      return { method: 'GET', path: statsPath('/stats/providers', args?.hours) }
    case 'getStatsByApiKey':
      return { method: 'GET', path: statsPath('/stats/api-keys', args?.hours) }
    case 'listProviderAllowances':
      return { method: 'GET', path: '/provider-allowances' }
    case 'refreshProviderAllowances':
      return { method: 'POST', path: '/provider-allowances/refresh' }
    case 'refreshProviderAllowance':
      return { method: 'POST', path: `/provider-allowances/${encodeURIComponent(String(args?.providerId))}/refresh` }
    case 'getSetting':
      return { method: 'GET', path: `/settings/${args?.key}` }
    case 'setSetting':
      return { method: 'PUT', path: `/settings/${args?.key}`, body: { value: args?.value } }
    case 'getGatewayStatus':
      return { method: 'GET', path: '/status' }
    default:
      throw new Error(`Unknown Stravia Admin operation: ${command}`)
  }
}

function statsPath(path: string, hours: unknown): string {
  return hours == null ? path : `${path}?hours=${hours}`
}

async function request<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const mapping = mapRequest(command, args)
  const headers = new Headers()
  const token = getAdminToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)
  if (mapping.body) headers.set('Content-Type', 'application/json')

  const response = await fetch(`${await resolveApiBase()}${mapping.path}`, {
    method: mapping.method,
    headers,
    body: mapping.body ? (typeof mapping.body === 'string' ? mapping.body : JSON.stringify(mapping.body)) : undefined,
  })

  if (response.status === 401 && window.location.pathname !== '/login') {
    clearAdminToken()
    window.location.assign('/login')
    throw new Error(m.frontend_error_authentication_required())
  }

  const text = await response.text()
  const payload = text ? parseJson(text) : undefined
  const responsePayload = payload !== null && typeof payload === 'object' ? (payload as ResponsePayload) : undefined
  if (!response.ok) {
    const message = typeof responsePayload?.error === 'string' ? responsePayload.error : `HTTP ${response.status}`
    const error = new Error(message) as Error & { code?: string; params?: Record<string, unknown> }
    if (responsePayload?.code) error.code = responsePayload.code
    if (responsePayload?.params) error.params = responsePayload.params
    throw error
  }

  if (typeof responsePayload?.error === 'string' && responsePayload.error.trim()) {
    throw new Error(responsePayload.error)
  }
  return responsePayload && 'data' in responsePayload ? (responsePayload.data as T) : (payload as T)
}

function parseJson(value: string): unknown {
  try {
    return JSON.parse(value)
  } catch {
    throw new Error(m.frontend_error_invalid_admin_json())
  }
}

export const admin = {
  connectClients: {
    preview: (input: ConnectClientApplyRequest) => request<ConnectClientApplyPlan>('previewConnectClient', { input }),
  },
  providers: {
    vendors: () => request<VendorMetadata[]>('listVendorMetadata'),
    list: () => request<Provider[]>('listProviders'),
    create: (input: CreateProvider) => request<Provider>('createProvider', { input }),
    previewBaseUrl: (vendorId: string, adapterCredentials: Record<string, string>, baseUrl?: string) =>
      request<{ base_url: string }>('previewProviderBaseUrl', { vendorId, adapterCredentials, baseUrl }),
    copy: (id: string, options: Record<string, unknown> = {}) => request<Provider>('copyProvider', { id, options }),
    update: (id: string, input: UpdateProvider) => request<Provider>('updateProvider', { id, input }),
    delete: (id: string) => request<void>('deleteProvider', { id }),
    test: (id: string) => request<TestResult>('testProvider', { id }),
    testModels: (id: string) => request<string[]>('testProviderModels', { id }),
    capabilityDrifts: () => request<ImageCapabilityDrift[]>('listImageCapabilityDrifts'),
    models: (id: string) => request<ProviderModelList>('listProviderModels', { id }),
    syncModels: (id: string) => request<ProviderModelSyncSummary>('syncProviderModels', { id }),
    prepareModel: (id: string, modelId: string, templateId?: string) =>
      request<PreparedProviderModel>('prepareProviderModel', { id, modelId, templateId }),
    model: (id: string, modelId: string) => request<ProviderModelDetail>('getProviderModel', { id, modelId }),
    createManualModel: (id: string, modelId: string, metadataJson: string) =>
      request<ProviderModelDetail>('createManualProviderModel', { id, modelId, metadataJson }),
    updateModel: (id: string, modelId: string, metadataJson: string, revision: number) =>
      request<ProviderModelDetail>('updateProviderModel', { id, modelId, metadataJson, revision }),
    updateModelSelection: (id: string, modelId: string, policy: ProviderModelSelectionPolicy, revision: number) =>
      request<ProviderModelDetail>('updateProviderModelSelection', { id, modelId, policy, revision }),
    reimportModel: (id: string, modelId: string, revision: number) =>
      request<ProviderModelDetail>('reimportProviderModel', { id, modelId, revision }),
    deleteManualModel: (id: string, modelId: string) => request<void>('deleteManualProviderModel', { id, modelId }),
    capabilities: (providerId: string, model: string) =>
      request<ModelCapabilities>('getModelCapabilities', { providerId, model }),
    oauthStatus: (id: string) => request<ProviderOAuthStatusData>('getProviderOAuthStatus', { id }),
    reconnectOAuth: (id: string) => request<void>('reconnectProviderOAuth', { id }),
    logoutOAuth: (id: string) => request<void>('logoutProviderOAuth', { id }),
    bindOAuth: (providerId: string, sessionId: string) => request<void>('bindProviderOAuth', { providerId, sessionId }),
    createOAuth: (sessionId: string, input: CreateProvider) =>
      request<Provider>('createOAuthProvider', { sessionId, input }),
  },
  catalog: {
    providers: async () => (await request<CatalogProviderList>('listCatalogProviders')).providers,
    canonicalModels: () => request<CanonicalModelList>('listCanonicalModels'),
    refresh: () => request<CatalogRefreshSummary>('refreshCatalog'),
  },
  webAccess: {
    providers: {
      list: () => request<WebProvider[]>('listWebProviders'),
      create: (input: CreateWebProvider) => request<WebProvider>('createWebProvider', { input }),
      update: (id: string, input: UpdateWebProvider) => request<WebProvider>('updateWebProvider', { id, input }),
      delete: (id: string) => request<void>('deleteWebProvider', { id }),
      test: (id: string) => request<TestResult>('testWebProvider', { id }),
    },
    settings: {
      get: () => request<WebAccessSettings>('getWebAccessSettings'),
      update: (input: WebAccessSettings) => request<WebAccessSettings>('updateWebAccessSettings', { input }),
    },
  },
  webSearch: {
    config: {
      get: () => request<WebSearchConfigView>('getWebSearchConfig'),
      update: (input: UpdateWebSearchConfig) => request<WebSearchConfigView>('updateWebSearchConfig', { input }),
    },
    eligibleModels: () => request<EligibleSearchModel[]>('listEligibleWebSearchModels'),
    compatibleCodexProviders: () => request<CompatibleCodexProvider[]>('listCompatibleCodexSearchProviders'),
  },
  mediaUnderstanding: {
    get: () => request<MediaUnderstandingConfigView>('getMediaUnderstandingConfig'),
    update: (input: UpdateMediaUnderstandingConfig) =>
      request<MediaUnderstandingConfigView>('updateMediaUnderstandingConfig', { input }),
  },
  oauth: {
    init: (vendor: string, useProxy: boolean, callbackMode: OAuthCallbackMode, locale: Locale) =>
      request<OAuthSessionInitData>('initOAuthSession', { vendor, useProxy, callbackMode, locale }),
    status: (sessionId: string) => request<OAuthSessionStatusData>('getOAuthSessionStatus', { sessionId }),
    cancel: (sessionId: string) => request<void>('cancelOAuthSession', { sessionId }),
    updateProxy: (sessionId: string, useProxy: boolean) =>
      request<OAuthSessionStatusData>('updateOAuthSessionProxy', { sessionId, useProxy }),
    complete: (sessionId: string, callbackUrl: string, metadata?: Record<string, unknown>) =>
      request<void>('completeOAuthSession', { sessionId, callbackUrl, metadata }),
  },
  models: {
    list: () => request<Route[]>('listModels'),
    get: (routeId: string) => request<Route>('getModel', { routeId }),
    create: (input: CreateRoute) => request<Route>('createModel', { input }),
    bind: (input: BindRouteInput) => request<Route>('bindRoute', { input }),
    unbind: (input: UnbindRouteInput) => request<Route | null>('unbindRoute', { input }),
    update: (routeId: string, input: UpdateRoute) => request<Route>('updateModel', { routeId, input }),
    delete: (routeId: string) => request<void>('deleteModel', { routeId }),
    resetThinkingMapping: (routeId: string, targetId: string, level: ThinkingLevel) =>
      request<Route>('resetTargetThinkingMapping', { routeId, targetId, level }),
    regenerateThinkingMap: (routeId: string, targetId: string) =>
      request<Route>('regenerateTargetThinkingMap', { routeId, targetId }),
  },
  apiKeys: {
    list: () => request<ApiKey[]>('listApiKeys'),
    create: (input: CreateApiKey) => request<ApiKey>('createApiKey', { input }),
    update: (id: string, input: UpdateApiKey) => request<ApiKey>('updateApiKey', { id, input }),
    delete: (id: string) => request<void>('deleteApiKey', { id }),
  },
  logs: {
    query: (query: LogQuery) => request<LogPage>('queryLogs', { query }),
    get: (id: string) => request<LogPage['items'][number]>('getLog', { id }),
    clear: () => request<void>('clearLogs'),
  },
  stats: {
    overview: (hours?: number) => request<StatsOverview>('getStatsOverview', { hours }),
    hourly: (hours?: number) => request<StatsHourly[]>('getStatsHourly', { hours }),
    models: (hours?: number) => request<ModelStats[]>('getStatsByModel', { hours }),
    providers: (hours?: number) => request<ProviderStats[]>('getStatsByProvider', { hours }),
    apiKeys: (hours?: number) => request<ApiKeyStats[]>('getStatsByApiKey', { hours }),
  },
  allowances: {
    list: () => request<ProviderAllowanceSnapshot[]>('listProviderAllowances'),
    refreshAll: () => request<ProviderAllowanceSnapshot[]>('refreshProviderAllowances'),
    refresh: (providerId: string) => request<ProviderAllowanceSnapshot>('refreshProviderAllowance', { providerId }),
  },
  settings: {
    get: (key: string) => request<string | null>('getSetting', { key }),
    set: (key: string, value: string) => request<void>('setSetting', { key, value }),
    status: () => request<GatewayStatus>('getGatewayStatus'),
  },
}

export async function proxyBase(): Promise<string> {
  if (!isTauri) return window.location.origin
  return (await resolveApiBase()).slice(0, -'/api/v1'.length)
}

export async function catalogLogoUrl(providerId: string): Promise<string> {
  return `${await resolveApiBase()}/catalog/providers/${encodeURIComponent(providerId)}/logo`
}
