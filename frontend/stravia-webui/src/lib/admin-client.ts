import type { StatsWindow } from '#lib/types/stats.js'
import { apiBase, authenticatedFetch, decodeAdmin, isTauri } from '#lib/auth.js'
import type { ConnectClientApplyPlan } from '#lib/connect-client-apply.js'
import type { ConnectClientApplyRequest } from '#lib/connect.js'
import type { Locale } from '#lib/paraglide/runtime.js'
import type { UpdateStatus } from '#lib/product-update.js'
import type {
  ApiKey,
  CredentialRuleCatalog,
  CredentialMatch,
  CustomCredentialRule,
  CustomCredentialRuleInput,
  CredentialDiscoveryQuery,
  CredentialDiscoveryPage,
  ProviderModelDetail,
  PreparedProviderModel,
  ProviderModelList,
  ProviderModelSelectionPolicy,
  ProviderModelSyncSummary,
  ProviderModelSyncState,
  UpdateProviderModelSyncState,
  ModelChangeQuery,
  ModelChangePage,
  ModelChangeRecord,
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
  RootChangesQuery,
  RootChangesPage,
  ClientErrorQuery,
  ClientErrorPage,
  ClientErrorDetail,
  ClientErrorSummary,
  UpstreamErrorQuery,
  UpstreamErrorPage,
  UpstreamErrorDetail,
  InteractionDetail,
  InteractionEventsQuery,
  InteractionEventsPage,
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
  ProviderMetadata,
  ProviderConfigurationPreview,
  ProviderConfigurationPreviewInput,
  ProviderConnectionCheckInput,
  ProviderConnectionCheckResult,
  WebAccessSettings,
  WebAccessBrowserState,
  WebProvider,
  WebSearchConfigView,
  UpdateWebSearchConfig,
  EligibleSearchModel,
  ExternalSearchRoute,
  MediaUnderstandingConfigView,
  MediaUnderstandingKind,
  UpdateMediaUnderstandingConfig,
  MediaGenerationConfig,
  MediaGenerationConfigView,
  EligibleMediaGenerationRoute,
  ThinkingLevel,
  ProviderAllowanceSnapshot,
  ProviderAllowanceGuardUpdate,
  ProviderAllowanceTarget,
  ConfirmPluginUpdate,
  PluginPreview,
  PluginSummary,
} from '#lib/types.js'

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

export interface ProxySettings {
  url: string
  bypass: string
  force_http1: boolean
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

function statsPath(path: string, window?: StatsWindow, bucket?: number): string {
  return `${path}${queryString({ ...window, bucket })}`
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
    customRules: {
      list: () => request<CustomCredentialRule[]>('GET', '/reversible-redaction/custom-rules'),
      create: (input: CustomCredentialRuleInput) =>
        request<CustomCredentialRule>('POST', '/reversible-redaction/custom-rules', input),
      update: (id: string, input: CustomCredentialRuleInput) =>
        request<CustomCredentialRule>('PUT', `/reversible-redaction/custom-rules/${encodeURIComponent(id)}`, input),
      delete: (id: string) => request<void>('DELETE', `/reversible-redaction/custom-rules/${encodeURIComponent(id)}`),
    },
  },
  connectClients: {
    preview: (input: ConnectClientApplyRequest) =>
      request<ConnectClientApplyPlan>('POST', '/connect-clients/preview', input),
  },
  modelChanges: {
    list: (query: ModelChangeQuery = {}) => request<ModelChangePage>('GET', `/model-changes${queryString(query)}`),
    get: (id: string) => request<ModelChangeRecord>('GET', `/model-changes/${encodeURIComponent(id)}`),
  },
  providers: {
    descriptors: () => request<ProviderMetadata[]>('GET', '/vendors'),
    choices: () => request<ProviderMetadata[]>('GET', '/vendors?selectable=true'),
    list: () => request<Provider[]>('GET', '/providers'),
    create: (input: CreateProvider) => request<Provider>('POST', '/providers', input),
    previewConfiguration: (input: ProviderConfigurationPreviewInput) =>
      request<ProviderConfigurationPreview>('POST', '/providers/configuration-preview', input),
    copy: (id: string, options: Record<string, unknown> = {}) =>
      request<Provider>('POST', `/providers/${id}/copy`, options),
    update: (id: string, input: UpdateProvider) => request<Provider>('PUT', `/providers/${id}`, input),
    delete: (id: string) => request<void>('DELETE', `/providers/${id}`),
    test: (id: string, input: ProviderConnectionCheckInput = {}) =>
      request<ProviderConnectionCheckResult>('POST', `/providers/${id}/test`, input),
    testModels: (id: string) => request<string[]>('GET', `/providers/${id}/test-models`),
    capabilityDrifts: () => request<ImageCapabilityDrift[]>('GET', '/providers/image-capability-drifts'),
    models: (id: string) => request<ProviderModelList>('GET', `/providers/${id}/models`),
    syncModels: (id: string) => request<ProviderModelSyncSummary>('POST', `/providers/${id}/models/sync`),
    syncState: (id: string) => request<ProviderModelSyncState>('GET', `/providers/${id}/models/sync-state`),
    updateSyncState: (id: string, input: UpdateProviderModelSyncState) =>
      request<ProviderModelSyncState>('PUT', `/providers/${id}/models/sync-state`, input),
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
    browser: {
      get: () => request<WebAccessBrowserState>('GET', '/web-access/browser'),
      set: (path: string | null) => request<WebAccessBrowserState>('PUT', '/web-access/browser', { path }),
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
    update: (kind: MediaUnderstandingKind, input: UpdateMediaUnderstandingConfig) =>
      request<MediaUnderstandingConfigView>('PUT', `/media-understanding/${kind}`, input),
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
    clientErrors: (query: ClientErrorQuery) =>
      request<ClientErrorPage>('GET', `/observations/client-errors${queryString(query)}`),
    clientError: (kind: ClientErrorSummary['kind'], id: string) =>
      request<ClientErrorDetail>(
        'GET',
        `/observations/client-errors/${encodeURIComponent(kind)}/${encodeURIComponent(id)}`,
      ),
    upstreamErrors: (query: UpstreamErrorQuery) =>
      request<UpstreamErrorPage>('GET', `/observations/upstream-errors${queryString(query)}`),
    upstreamError: (attemptId: string) =>
      request<UpstreamErrorDetail>('GET', `/observations/upstream-errors/${encodeURIComponent(attemptId)}`),
    forest: (query: ForestQuery) => request<ForestPage>('GET', `/observations/interactions${queryString(query)}`),
    changes: (query: RootChangesQuery) => request<RootChangesPage>('POST', '/observations/interactions/changes', query),
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
  performance: {
    metrics: async (): Promise<Blob> => {
      const response = await authenticatedFetch('/performance/snapshot')
      if (!response.ok) await decodeAdmin(response)
      return response.blob()
    },
    timeline: async (): Promise<Blob> => {
      const response = await authenticatedFetch('/performance/timeline')
      if (!response.ok) await decodeAdmin(response)
      return response.blob()
    },
  },
  stats: {
    overview: (window?: StatsWindow) => request<StatsOverview>('GET', statsPath('/stats/overview', window)),
    series: (window?: StatsWindow, bucket?: number) =>
      request<StatsSeries[]>('GET', statsPath('/stats/series', window, bucket)),
    models: (window?: StatsWindow) => request<ModelStats[]>('GET', statsPath('/stats/models', window)),
    providers: (window?: StatsWindow) => request<ProviderStats[]>('GET', statsPath('/stats/providers', window)),
    apiKeys: (window?: StatsWindow) => request<ApiKeyStats[]>('GET', statsPath('/stats/api-keys', window)),
  },
  allowances: {
    list: () => request<ProviderAllowanceTarget[]>('GET', '/provider-allowances'),
    get: (providerId: string) =>
      request<ProviderAllowanceSnapshot>('GET', `/provider-allowances/${encodeURIComponent(providerId)}`),
    refresh: (providerId: string) =>
      request<ProviderAllowanceSnapshot>('POST', `/provider-allowances/${providerId}/refresh`),
    replaceGuards: (providerId: string, keys: string[]) =>
      request<ProviderAllowanceGuardUpdate>('PUT', `/provider-allowances/${encodeURIComponent(providerId)}/guards`, {
        keys,
      }),
  },
  settings: {
    proxy: async (): Promise<ProxySettings> => {
      const value = await request<string | null>('GET', '/settings/outbound_proxy')
      return value === null ? { url: '', bypass: '', force_http1: false } : (JSON.parse(value) as ProxySettings)
    },
    saveProxy: (settings: ProxySettings) =>
      request<void>('PUT', '/settings/outbound_proxy', { value: JSON.stringify(settings) }),
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
