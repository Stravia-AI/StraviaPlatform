import { apiBase, authenticatedFetch, decodeAdmin, isTauri } from '$lib/auth'
import type { ConnectClientApplyPlan } from '$lib/connect-client-apply'
import type { ConnectClientApplyRequest } from '$lib/connect'
import type { Locale } from '$lib/paraglide/runtime.js'
import type { UpdateStatus } from '$lib/product-update'
import type {
  ApiKey,
  CredentialRuleCatalog,
  CredentialMatch,
  CredentialDiscoveryQuery,
  CredentialDiscoveryPage,
  ProviderModelDetail,
  PreparedProviderModel,
  ProviderModelList,
  ProviderModelSelectionPolicy,
  ProviderModelSyncSummary,
  CanonicalModelList,
  ApiKeyStats,
  CreateApiKey,
  BindRouteInput,
  CreateRoute,
  CreateProvider,
  CreateWebProvider,
  GatewayStatus,
  ForestPage,
  ForestQuery,
  FailedRequestQuery,
  FailedRequestPage,
  FailedRequestDetail,
  FailedRequestSummary,
  InteractionDetail,
  InteractionEventsQuery,
  InteractionEventsPage,
  InteractionSnapshot,
  DebugState,
  ClearHistoryResult,
  DownloadTicket,
  BundleResourceKind,
  Route,
  ImageCapabilityDrift,
  ModelCapabilities,
  ModelStats,
  OAuthCallbackMode,
  OAuthCandidateConfiguration,
  OAuthSessionInitData,
  OAuthSessionStatusData,
  Provider,
  ProviderOAuthStatusData,
  ProviderStats,
  StatsOverview,
  StatsSeries,
  TargetRuntimeStatus,
  TestResult,
  UpdateApiKey,
  UnbindRouteInput,
  UpdateRoute,
  UpdateProvider,
  UpdateWebProvider,
  ProviderDescriptor,
  ProviderConfigurationPreview,
  ProviderConfigurationPreviewInput,
  WebAccessSettings,
  WebProvider,
  WebSearchConfigView,
  UpdateWebSearchConfig,
  EligibleSearchModel,
  ExternalSearchRoute,
  MediaUnderstandingConfigView,
  UpdateMediaUnderstandingConfig,
  MediaGenerationConfig,
  MediaGenerationConfigView,
  EligibleMediaGenerationRoute,
  ThinkingLevel,
  ProviderAllowanceSnapshot,
  ProviderAllowanceTarget,
  ConfirmPluginUpdate,
  PluginPreview,
  PluginSummary,
} from '$lib/types'

export { isTauri }

export interface ArtifactS3Settings {
  endpoint: string
  region: string
  bucket: string
  access_key_id: string
  secret_access_key: string
  session_token: string | null
  credentials_expires_at: number | null
}

export interface ArtifactSettings {
  client_base_url: string
  external_signed_downloads: boolean
  file_public_base_url: string | null
  upload_prompt_injection: boolean
  s3: ArtifactS3Settings | null
}

type HttpMethod = 'DELETE' | 'GET' | 'POST' | 'PUT'

function queryString(query: object): string {
  const params = new URLSearchParams()
  for (const [key, value] of Object.entries(query)) {
    if (value != null && value !== '') params.set(key, String(value))
  }
  return params.size > 0 ? `?${params}` : ''
}

function statsPath(path: string, hours?: number): string {
  return hours == null ? path : `${path}?hours=${hours}`
}

async function request<T>(method: HttpMethod, path: string, body?: unknown): Promise<T> {
  const headers = new Headers()
  if (method !== 'GET') headers.set('X-Stravia-CSRF', '1')
  if (body !== undefined) headers.set('Content-Type', 'application/json')
  const response = await authenticatedFetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  })
  return decodeAdmin<T>(response)
}

async function uploadWasm<T>(path: string, file: File): Promise<T> {
  const response = await authenticatedFetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/wasm', 'X-Stravia-CSRF': '1' },
    body: file,
  })
  return decodeAdmin<T>(response)
}

export const admin = {
  vendorPlugins: {
    list: () => request<PluginSummary[]>('GET', '/vendor-plugins'),
    import: (file: File) => uploadWasm<PluginPreview>('/vendor-plugins/import', file),
    confirm: (input: ConfirmPluginUpdate) => request<PluginSummary>('POST', '/vendor-plugins/confirm', input),
    restoreBuiltin: (vendorId: string) =>
      request<PluginPreview>('POST', `/vendor-plugins/${encodeURIComponent(vendorId)}/restore`),
    uninstall: (vendorId: string) => request<void>('DELETE', `/vendor-plugins/${encodeURIComponent(vendorId)}`),
  },
  credentialProtection: {
    rules: () => request<CredentialRuleCatalog>('GET', '/reversible-redaction/rules'),
    discoveries: (query: CredentialDiscoveryQuery = {}) =>
      request<CredentialDiscoveryPage>('GET', `/reversible-redaction/discoveries${queryString(query)}`),
    test: (text: string) => request<{ matches: CredentialMatch[] }>('POST', '/reversible-redaction/test', { text }),
  },
  connectClients: {
    preview: (input: ConnectClientApplyRequest) =>
      request<ConnectClientApplyPlan>('POST', '/connect-clients/preview', input),
  },
  providers: {
    descriptors: () => request<ProviderDescriptor[]>('GET', '/vendors'),
    list: () => request<Provider[]>('GET', '/providers'),
    create: (input: CreateProvider) => request<Provider>('POST', '/providers', input),
    previewConfiguration: (input: ProviderConfigurationPreviewInput) =>
      request<ProviderConfigurationPreview>('POST', '/providers/configuration-preview', input),
    copy: (id: string, options: Record<string, unknown> = {}) =>
      request<Provider>('POST', `/providers/${id}/copy`, options),
    update: (id: string, input: UpdateProvider) => request<Provider>('PUT', `/providers/${id}`, input),
    delete: (id: string) => request<void>('DELETE', `/providers/${id}`),
    test: (id: string) => request<TestResult>('GET', `/providers/${id}/test`),
    testModels: (id: string) => request<string[]>('GET', `/providers/${id}/test-models`),
    capabilityDrifts: () => request<ImageCapabilityDrift[]>('GET', '/providers/image-capability-drifts'),
    models: (id: string) => request<ProviderModelList>('GET', `/providers/${id}/models`),
    syncModels: (id: string) => request<ProviderModelSyncSummary>('POST', `/providers/${id}/models/sync`),
    prepareModel: (id: string, modelId: string, templateId?: string) =>
      request<PreparedProviderModel>('POST', `/providers/${id}/model/prepare`, {
        model_id: modelId,
        template_id: templateId,
      }),
    model: (id: string, modelId: string) =>
      request<ProviderModelDetail>('GET', `/providers/${id}/model?model=${encodeURIComponent(modelId)}`),
    createManualModel: (id: string, modelId: string, metadataJson: string, templateId?: string) =>
      request<ProviderModelDetail>(
        'POST',
        `/providers/${id}/models`,
        `{"model_id":${JSON.stringify(modelId)},"metadata":${metadataJson},"template_id":${JSON.stringify(templateId ?? null)}}`,
      ),
    updateModel: (id: string, modelId: string, metadataJson: string, revision: number) =>
      request<ProviderModelDetail>(
        'PUT',
        `/providers/${id}/model`,
        `{"model_id":${JSON.stringify(modelId)},"metadata":${metadataJson},"revision":${revision}}`,
      ),
    updateModelSelection: (id: string, modelId: string, policy: ProviderModelSelectionPolicy, revision: number) =>
      request<ProviderModelDetail>('PUT', `/providers/${id}/model/selection`, { model_id: modelId, policy, revision }),
    reimportModel: (id: string, modelId: string, revision: number) =>
      request<ProviderModelDetail>('POST', `/providers/${id}/model/reimport`, { model_id: modelId, revision }),
    deleteManualModel: (id: string, modelId: string) =>
      request<void>('DELETE', `/providers/${id}/model?model=${encodeURIComponent(modelId)}`),
    capabilities: (providerId: string, model: string) =>
      request<ModelCapabilities>(
        'GET',
        `/providers/${providerId}/model-capabilities?model=${encodeURIComponent(model)}`,
      ),
    oauthStatus: (id: string) => request<ProviderOAuthStatusData>('GET', `/providers/${id}/oauth/status`),
    reconnectOAuth: (id: string) => request<void>('POST', `/providers/${id}/oauth/reconnect`),
    logoutOAuth: (id: string) => request<void>('POST', `/providers/${id}/oauth/logout`),
    bindOAuth: (providerId: string, sessionId: string) =>
      request<void>('POST', `/providers/${providerId}/oauth/bind`, { session_id: sessionId }),
    createOAuth: (sessionId: string, input: CreateProvider) =>
      request<Provider>('POST', '/providers/oauth', { session_id: sessionId, input }),
  },
  catalog: { canonicalModels: () => request<CanonicalModelList>('GET', '/catalog/models') },
  webAccess: {
    providers: {
      list: () => request<WebProvider[]>('GET', '/web-providers'),
      create: (input: CreateWebProvider) => request<WebProvider>('POST', '/web-providers', input),
      update: (id: string, input: UpdateWebProvider) => request<WebProvider>('PUT', `/web-providers/${id}`, input),
      delete: (id: string) => request<void>('DELETE', `/web-providers/${id}`),
      test: (id: string) => request<TestResult>('POST', `/web-providers/${id}/test`),
    },
    settings: {
      get: () => request<WebAccessSettings>('GET', '/web-access/settings'),
      update: (input: WebAccessSettings) => request<WebAccessSettings>('PUT', '/web-access/settings', input),
    },
  },
  webSearch: {
    config: {
      get: () => request<WebSearchConfigView>('GET', '/web-search/config'),
      update: (input: UpdateWebSearchConfig) => request<WebSearchConfigView>('PUT', '/web-search/config', input),
    },
    eligibleModels: () => request<EligibleSearchModel[]>('GET', '/web-search/eligible-models'),
    externalRoutes: () => request<ExternalSearchRoute[]>('GET', '/web-search/external-routes'),
  },
  mediaUnderstanding: {
    get: () => request<MediaUnderstandingConfigView>('GET', '/media-understanding'),
    update: (input: UpdateMediaUnderstandingConfig) =>
      request<MediaUnderstandingConfigView>('PUT', '/media-understanding', input),
  },
  mediaGeneration: {
    config: {
      get: () => request<MediaGenerationConfigView>('GET', '/media-generation/config'),
      update: (input: MediaGenerationConfig) =>
        request<MediaGenerationConfigView>('PUT', '/media-generation/config', input),
    },
    eligibleRoutes: () => request<EligibleMediaGenerationRoute[]>('GET', '/media-generation/eligible-routes'),
  },
  oauth: {
    init: (
      vendorId: string,
      channel: string,
      configuration: OAuthCandidateConfiguration,
      useProxy: boolean,
      callbackMode: OAuthCallbackMode,
      locale: Locale,
    ) =>
      request<OAuthSessionInitData>('POST', '/oauth/sessions/init', {
        vendor_id: vendorId,
        channel,
        use_proxy: useProxy,
        callback_mode: callbackMode,
        locale,
        ...configuration,
      }),
    status: (sessionId: string) => request<OAuthSessionStatusData>('GET', `/oauth/sessions/${sessionId}/status`),
    cancel: (sessionId: string) => request<void>('POST', `/oauth/sessions/${sessionId}/cancel`),
    updateProxy: (sessionId: string, useProxy: boolean) =>
      request<OAuthSessionStatusData>('PUT', `/oauth/sessions/${sessionId}/proxy`, { use_proxy: useProxy }),
    complete: (sessionId: string, type: 'callback_url' | 'manual', value: string) =>
      request<void>('POST', `/oauth/sessions/${sessionId}/complete`, { input: { type, value } }),
  },
  models: {
    list: () => request<Route[]>('GET', '/models'),
    get: (routeId: string) => request<Route>('GET', `/models/${encodeURIComponent(routeId)}`),
    create: (input: CreateRoute) => request<Route>('POST', '/models', input),
    bind: (input: BindRouteInput) => request<Route>('POST', '/models/bind', input),
    unbind: (input: UnbindRouteInput) => request<Route | null>('POST', '/models/unbind', input),
    update: (routeId: string, input: UpdateRoute) =>
      request<Route>('PUT', `/models/${encodeURIComponent(routeId)}`, input),
    delete: (routeId: string) => request<void>('DELETE', `/models/${encodeURIComponent(routeId)}`),
    resetThinkingMapping: (routeId: string, targetId: string, level: ThinkingLevel) =>
      request<Route>('POST', `/models/${encodeURIComponent(routeId)}/targets/${targetId}/thinking-map/reset`, {
        level,
      }),
    regenerateThinkingMap: (routeId: string, targetId: string) =>
      request<Route>('POST', `/models/${encodeURIComponent(routeId)}/targets/${targetId}/thinking-map/regenerate`),
    targetStatuses: (routeId: string) =>
      request<TargetRuntimeStatus[]>('GET', `/models/${encodeURIComponent(routeId)}/target-statuses`),
  },
  apiKeys: {
    list: () => request<ApiKey[]>('GET', '/api-keys'),
    create: (input: CreateApiKey) => request<ApiKey>('POST', '/api-keys', input),
    update: (id: string, input: UpdateApiKey) => request<ApiKey>('PUT', `/api-keys/${id}`, input),
    delete: (id: string) => request<void>('DELETE', `/api-keys/${id}`),
  },
  observations: {
    failures: (query: FailedRequestQuery) =>
      request<FailedRequestPage>('GET', `/observations/failed-requests${queryString(query)}`),
    failure: (kind: FailedRequestSummary['kind'], id: string) =>
      request<FailedRequestDetail>(
        'GET',
        `/observations/failed-requests/${encodeURIComponent(kind)}/${encodeURIComponent(id)}`,
      ),
    forest: (query: ForestQuery) => request<ForestPage>('GET', `/observations/interactions${queryString(query)}`),
    interactionSummary: (id: string, query: ForestQuery = {}) =>
      request<InteractionSnapshot>(
        'GET',
        `/observations/interactions/${encodeURIComponent(id)}/summary${queryString(query)}`,
      ),
    interaction: (id: string, query: ForestQuery = {}) =>
      request<InteractionDetail>(
        'GET',
        `/observations/interactions/${encodeURIComponent(id)}${queryString({
          provider: query.provider,
          model: query.model,
          api_key: query.api_key,
          status: query.status,
          min_tokens: query.min_tokens,
        })}`,
      ),
    interactionEvents: (id: string, query: InteractionEventsQuery) =>
      request<InteractionEventsPage>(
        'GET',
        `/observations/interactions/${encodeURIComponent(id)}/events${queryString(query) || '?'}`,
      ),
    debug: () => request<DebugState>('GET', '/observations/debug'),
    setDebug: (enabled: boolean) => request<DebugState>('PUT', '/observations/debug', { enabled, confirmed: enabled }),
    clearDebug: () => request<DebugState>('DELETE', '/observations/debug'),
    clearHistory: () => request<ClearHistoryResult>('DELETE', '/observations/history'),
    issueBundleTicket: (kind: BundleResourceKind, id: string, throughSequence?: number) =>
      request<DownloadTicket>(
        'POST',
        `/observations/${kind === 'rejected_request' ? 'rejections' : 'interactions'}/${encodeURIComponent(id)}/debug-bundle-tickets`,
        throughSequence == null ? {} : { through_sequence: throughSequence },
      ),
  },
  stats: {
    overview: (hours?: number) => request<StatsOverview>('GET', statsPath('/stats/overview', hours)),
    series: (hours?: number, bucket?: number, tzOffset?: number) => {
      const params = new URLSearchParams()
      params.set('hours', String(hours ?? 24))
      params.set('bucket', String(bucket ?? 3600))
      params.set('tz_offset', String(tzOffset ?? 0))
      return request<StatsSeries[]>('GET', `/stats/series?${params}`)
    },
    models: (hours?: number) => request<ModelStats[]>('GET', statsPath('/stats/models', hours)),
    providers: (hours?: number) => request<ProviderStats[]>('GET', statsPath('/stats/providers', hours)),
    apiKeys: (hours?: number) => request<ApiKeyStats[]>('GET', statsPath('/stats/api-keys', hours)),
  },
  allowances: {
    list: () => request<ProviderAllowanceTarget[]>('GET', '/provider-allowances'),
    get: (providerId: string) =>
      request<ProviderAllowanceSnapshot>('GET', `/provider-allowances/${encodeURIComponent(providerId)}`),
    refresh: (providerId: string) =>
      request<ProviderAllowanceSnapshot>('POST', `/provider-allowances/${providerId}/refresh`),
  },
  settings: {
    artifacts: async (): Promise<ArtifactSettings> => {
      const value = await request<string | null>('GET', '/settings/artifact_settings')
      return value === null
        ? {
            client_base_url: '',
            external_signed_downloads: false,
            file_public_base_url: null,
            upload_prompt_injection: false,
            s3: null,
          }
        : (JSON.parse(value) as ArtifactSettings)
    },
    saveArtifacts: (settings: ArtifactSettings) =>
      request<void>('PUT', '/settings/artifact_settings', { value: JSON.stringify(settings) }),
    get: (key: string) => request<string | null>('GET', `/settings/${key}`),
    set: (key: string, value: string) => request<void>('PUT', `/settings/${key}`, { value }),
    status: () => request<GatewayStatus>('GET', '/status'),
  },
  updates: {
    get: () => request<UpdateStatus>('GET', '/updates'),
    check: (mode: 'automatic' | 'manual') => request<UpdateStatus>('POST', '/updates/check', { mode }),
    skip: (version: string | null) => request<UpdateStatus>('PUT', '/updates/skipped-version', { version }),
  },
}

export async function proxyBase(): Promise<string> {
  if (!isTauri) return window.location.origin
  return (await apiBase()).slice(0, -'/api/v1'.length)
}

export async function catalogLogoUrl(providerId: string): Promise<string> {
  return `${await apiBase()}/catalog/providers/${encodeURIComponent(providerId)}/logo`
}
