import type { Page } from '@playwright/test'

const settingValues: Record<string, string> = {
  log_retention_days: '7',
  proxy_bypass: '',
  proxy_enabled: 'false',
  proxy_url: '',
}

export async function prepareApp(page: Page): Promise<void> {
  await page.addInitScript(() => {
    localStorage.setItem('stravia-admin-token', 'playwright-token')
    localStorage.setItem('stravia-locale', 'en-US')
    localStorage.setItem('stravia-sidebar-state', 'expanded')
    localStorage.setItem('stravia-theme', 'system')
  })

  await page.route('**/api/v1/**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')

    if (path === '/status') {
      await route.fulfill({ json: { data: { status: 'running' } } })
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
    if (path === '/catalog/providers') {
      await route.fulfill({
        json: {
          revision: 'test-catalog',
          generated_at: '2026-08-20T14:01:40Z',
          providers: [
            {
              id: 'openai',
              name: 'OpenAI',
              documentation_url: 'https://platform.openai.com/docs',
              protocol: 'openai-compatible',
              base_url: 'https://api.openai.com/v1',
              channels: [
                {
                  id: 'default',
                  label: 'Default',
                  protocol: 'openai-compatible',
                  base_url: 'https://api.openai.com/v1',
                  auth_mode: 'optional_api_key',
                  fingerprint: 'openai-default',
                },
                {
                  id: 'codex',
                  label: 'Codex',
                  protocol: 'open-responses',
                  base_url: 'https://chatgpt.com/backend-api/codex',
                  auth_mode: 'oauth',
                  fingerprint: 'openai-codex',
                },
              ],
            },
            {
              id: 'anthropic',
              name: 'Anthropic',
              documentation_url: 'https://docs.anthropic.com',
              protocol: 'anthropic-messages',
              base_url: 'https://api.anthropic.com',
              channels: [
                {
                  id: 'default',
                  label: 'Default',
                  protocol: 'anthropic-messages',
                  base_url: 'https://api.anthropic.com',
                  auth_mode: 'optional_api_key',
                  fingerprint: 'anthropic-default',
                },
                {
                  id: 'claude-code',
                  label: 'Claude Code',
                  protocol: 'anthropic-messages',
                  base_url: 'https://api.anthropic.com',
                  auth_mode: 'oauth',
                  fingerprint: 'anthropic-claude-code',
                },
              ],
            },
          ],
        },
      })
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
    if (path === '/catalog/refresh') {
      await route.fulfill({
        json: {
          revision: 'refreshed-catalog',
          generated_at: '2026-08-20T15:01:40Z',
          provider_count: 2,
          model_count: 4,
          changed: true,
        },
      })
      return
    }
    if (path === '/oauth/sessions/init') {
      await route.fulfill({
        json: {
          data: {
            session_id: 'oauth-session-1',
            vendor: 'codex',
            scheme: 'oauth_auth_code_pkce',
            auth_url: 'https://auth.openai.example/authorize',
            callback_mode: 'auto',
            listener_state: 'listening',
            listener_port: 1457,
            redirect_uri: 'http://localhost:1457/auth/callback',
            fallback_reason: null,
            expires_in: 600,
            interval: 2,
          },
        },
      })
      return
    }
    if (path === '/oauth/sessions/oauth-session-1/status') {
      await route.fulfill({
        json: {
          data: {
            status: 'pending',
            scheme: 'oauth_auth_code_pkce',
            auth_url: 'https://auth.openai.example/authorize',
            callback_mode: 'auto',
            listener_state: 'listening',
            listener_port: 1457,
            redirect_uri: 'http://localhost:1457/auth/callback',
            fallback_reason: null,
            expires_in: 600,
            interval: 2,
          },
        },
      })
      return
    }

    await route.fulfill({ json: { data: [] } })
  })
}
