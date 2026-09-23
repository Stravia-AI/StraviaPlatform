import type { Page } from '@playwright/test'
import type { UpdateStatus } from '../src/lib/product-update'
import type { ProviderDescriptor } from '../src/lib/types'

const codexManualInput = {
  type: 'callback_url',
  label: { 'en-US': 'Callback URL', 'zh-CN': '回调 URL' },
  description: { 'en-US': 'Paste the full callback URL after authorization.', 'zh-CN': '授权后粘贴完整的回调 URL。' },
  secret: false,
} as const

const providerDescriptors = [
  {
    provider_id: 'openai',
    catalog_id: 'openai',
    display_name: 'OpenAI',
    description: 'OpenAI API connections.',
    channels: [
      {
        id: 'default',
        name: { 'en-US': 'OpenAI API', 'zh-CN': 'OpenAI API' },
        description: { 'en-US': 'API key', 'zh-CN': 'API 密钥' },
        auth: null,
        protocol: 'openai-compatible',
        default_base_url: 'https://api.openai.com/v1',
        capabilities: ['infer', 'compact', 'model_discovery', 'config_validation'],
        model_capabilities: [],
        search_model_required: false,
      },
    ],
    capabilities: ['infer', 'compact', 'model_discovery', 'config_validation'],
    config_fields: [
      {
        key: 'api_key',
        label: { 'en-US': 'API key', 'zh-CN': 'API 密钥' },
        description: { 'en-US': 'OpenAI API key.', 'zh-CN': 'OpenAI API 密钥。' },
        kind: { type: 'string', multiline: false },
        required: false,
        secret: true,
      },
    ],
    config_groups: [],
    network: { base_url_field: null, extra_origins: [], field_origins: [] },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
  },
  {
    provider_id: 'openai-codex',
    catalog_id: 'openai',
    display_name: 'OpenAI Codex',
    description: 'Codex OAuth account connections.',
    channels: [
      {
        id: 'codex',
        name: { 'en-US': 'Codex', 'zh-CN': 'Codex' },
        description: { 'en-US': 'OAuth account', 'zh-CN': 'OAuth 账号' },
        auth: {
          flow: 'authorization_code',
          callback: {
            bind_host: '127.0.0.1',
            redirect_host: 'localhost',
            path: '/auth/callback',
            port: { kind: 'fixed', primary: 1455, fallback: 1456 },
            manual_redirect_uri: 'http://localhost:1457/auth/callback',
            cancel_path: '/cancel',
          },
          manual_input: codexManualInput,
        },
        protocol: 'open-responses',
        default_base_url: 'https://chatgpt.com/backend-api/codex',
        capabilities: [
          'infer',
          'compact',
          'search',
          'media_image',
          'auth_oauth',
          'model_discovery',
          'allowance',
          'config_validation',
        ],
        model_capabilities: [],
        search_model_required: true,
      },
    ],
    capabilities: [
      'infer',
      'compact',
      'search',
      'media_image',
      'auth_oauth',
      'model_discovery',
      'allowance',
      'config_validation',
    ],
    config_fields: [],
    config_groups: [],
    network: {
      base_url_field: null,
      extra_origins: [
        { scheme: 'https', host: 'auth.openai.com' },
        { scheme: 'https', host: 'chatgpt.com' },
      ],
      field_origins: [],
    },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
  },
  {
    provider_id: 'anthropic',
    catalog_id: 'anthropic',
    display_name: 'Anthropic',
    description: 'Anthropic API and Claude Code OAuth account connections.',
    channels: [
      {
        id: 'default',
        name: { 'en-US': 'Anthropic API', 'zh-CN': 'Anthropic API' },
        description: { 'en-US': 'API key', 'zh-CN': 'API 密钥' },
        auth: null,
        protocol: 'anthropic-messages',
        default_base_url: 'https://api.anthropic.com',
        capabilities: ['infer', 'compact', 'model_discovery', 'config_validation'],
        model_capabilities: [],
        search_model_required: false,
      },
      {
        id: 'claude-code',
        name: { 'en-US': 'Claude Code', 'zh-CN': 'Claude Code' },
        description: { 'en-US': 'OAuth account', 'zh-CN': 'OAuth 账号' },
        auth: {
          flow: 'authorization_code',
          callback: {
            bind_host: '127.0.0.1',
            redirect_host: 'localhost',
            path: '/auth/callback',
            port: { kind: 'dynamic' },
          },
          manual_input: null,
        },
        protocol: 'anthropic-messages',
        default_base_url: 'https://api.anthropic.com',
        capabilities: ['infer', 'compact', 'auth_oauth', 'model_discovery', 'config_validation'],
        model_capabilities: [],
        search_model_required: false,
      },
    ],
    capabilities: ['infer', 'compact', 'auth_oauth', 'model_discovery', 'config_validation'],
    config_fields: [
      {
        key: 'api_key',
        label: { 'en-US': 'API key', 'zh-CN': 'API 密钥' },
        description: {
          'en-US': 'Anthropic API key for the default channel.',
          'zh-CN': '默认渠道的 Anthropic API 密钥。',
        },
        kind: { type: 'string', multiline: false },
        required: false,
        secret: true,
      },
    ],
    config_groups: [],
    network: { base_url_field: null, extra_origins: [], field_origins: [] },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
  },
  {
    provider_id: 'openai-compatible',
    catalog_id: null,
    display_name: 'OpenAI Compatible',
    description: 'Bring your own OpenAI-compatible endpoint.',
    channels: [
      {
        id: 'default',
        name: { 'en-US': 'Default', 'zh-CN': '默认' },
        description: { 'en-US': 'Bring your own endpoint', 'zh-CN': '自定义端点' },
        auth: null,
        protocol: 'openai-compatible',
        default_base_url: null,
        capabilities: ['infer', 'model_discovery'],
        model_capabilities: [],
        search_model_required: false,
      },
    ],
    capabilities: ['infer', 'model_discovery'],
    config_fields: [
      {
        key: 'apiKey',
        label: { 'en-US': 'API key', 'zh-CN': 'API 密钥' },
        description: { 'en-US': 'Credential sent to the configured endpoint.', 'zh-CN': '发送给配置端点的凭据。' },
        kind: { type: 'string', multiline: false },
        required: true,
        secret: true,
      },
    ],
    config_groups: [],
    network: { base_url_field: null, extra_origins: [], field_origins: [] },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
  },
] satisfies ProviderDescriptor[]

const settingValues: Record<string, string> = {
  artifact_settings: JSON.stringify({
    client_base_url: 'https://client.example/stravia',
    external_signed_downloads: false,
    file_public_base_url: null,
    upload_prompt_injection: false,
    s3: null,
  }),
  log_retention_days: '7',
  proxy_bypass: '',
  proxy_enabled: 'false',
  proxy_url: '',
}

export async function prepareApp(page: Page): Promise<void> {
  let updateStatus: UpdateStatus = {
    current_version: '0.1.5',
    check_status: 'up-to-date',
    last_success_at: '2026-09-05T00:00:00Z',
    last_failure: null,
    available_update: null,
    skipped: false,
    download_supported: false,
  }
  await page.addInitScript(() => {
    localStorage.setItem('stravia-locale', 'en-US')
    if (localStorage.getItem('stravia-sidebar-state') === null) {
      localStorage.setItem('stravia-sidebar-state', 'expanded')
    }
    localStorage.setItem('stravia-theme', 'system')
  })

  await page.route('**/api/v1/**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')

    if (path === '/auth/state') {
      await route.fulfill({
        json: { mode: 'server', authenticated: true, setup_authorized: false, username: 'playwright-admin' },
      })
      return
    }
    if (path === '/status') {
      await route.fulfill({ json: { data: { status: 'running' } } })
      return
    }
    if (path === '/updates' || path === '/updates/check') {
      await route.fulfill({ json: { data: updateStatus } })
      return
    }
    if (path === '/updates/skipped-version') {
      const version = request.postDataJSON()?.version as string | null
      updateStatus = { ...updateStatus, skipped: version != null && version === updateStatus.available_update?.version }
      await route.fulfill({ json: { data: updateStatus } })
      return
    }

    if (path.startsWith('/settings/')) {
      const key = path.slice('/settings/'.length)
      await route.fulfill({ json: { data: request.method() === 'GET' ? (settingValues[key] ?? '') : null } })
      return
    }
    if (path.endsWith('/logo')) {
      await route.fulfill({
        contentType: 'image/svg+xml',
        body: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><path d="M0 0h1v1H0z"/></svg>',
      })
      return
    }
    if (path === '/vendors') {
      await route.fulfill({ json: { data: providerDescriptors } })
      return
    }
    if (path === '/providers/configuration-preview') {
      const input = request.postDataJSON() as { base_url?: string }
      await route.fulfill({ json: { data: { base_url: input.base_url ?? '', issues: [], network_permissions: [] } } })
      return
    }
    if (path === '/catalog/models') {
      await route.fulfill({
        json: {
          revision: 'test-catalog',
          generated_at: '2026-08-20T14:01:40Z',
          models: [
            { id: 'openai/gpt-5.4', name: 'GPT-5.4' },
            { id: 'openai/gpt-5.3-codex-spark', name: 'GPT-5.3 Codex Spark' },
            { id: 'anthropic/claude-opus-4.6', name: 'Claude Opus 4.6' },
          ],
        },
      })
      return
    }
    if (path === '/oauth/sessions/init') {
      await route.fulfill({
        json: {
          data: {
            session_id: 'oauth-session-1',
            vendor_id: 'openai-codex',
            channel: 'codex',
            flow: 'authorization_code',
            auth_url: 'https://auth.openai.example/authorize',
            callback_mode: 'auto',
            listener_state: 'listening',
            listener_port: 1457,
            redirect_uri: 'http://localhost:1457/auth/callback',
            fallback_reason: null,
            manual_input: codexManualInput,
            expires_in: 600,
            interval: 2,
          },
        },
      })
      return
    }
    if (path === '/observations/interactions') {
      const now = Date.now()
      await route.fulfill({
        json: {
          data: {
            anchor_at: now,
            window_index: 0,
            window_start: now - 86_400_000,
            window_end: now,
            roots: [],
            root_total: 0,
            next_cursor: null,
            snapshot_sequence: 0,
          },
        },
      })
      return
    }
    if (path === '/observations/rejections') {
      await route.fulfill({ json: { data: { items: [], total: 0, next_cursor: null, snapshot_sequence: 0 } } })
      return
    }
    if (path === '/observations/debug') {
      await route.fulfill({
        json: { data: { enabled: false, retained_bytes: 0, partial_trace_count: 0, retention_days: 7 } },
      })
      return
    }
    if (path === '/observations/events') {
      await route.fulfill({ contentType: 'text/event-stream', body: '' })
      return
    }
    if (path === '/oauth/sessions/oauth-session-1/status') {
      await route.fulfill({
        json: {
          data: {
            status: 'pending',
            auth_url: 'https://auth.openai.example/authorize',
            callback_mode: 'auto',
            listener_state: 'listening',
            listener_port: 1457,
            redirect_uri: 'http://localhost:1457/auth/callback',
            fallback_reason: null,
            manual_input: codexManualInput,
            expires_in: 600,
            interval: 2,
          },
        },
      })
      return
    }
    if (path === '/media-generation/config') {
      await route.fulfill({
        json: {
          data: {
            config: { enabled: false, image: { route_id: null } },
            validation: { valid: false, code: 'media_generation_route_missing', message: null },
          },
        },
      })
      return
    }
    if (path === '/media-generation/eligible-routes') {
      await route.fulfill({ json: { data: [] } })
      return
    }

    await route.fulfill({ json: { data: [] } })
  })
}
