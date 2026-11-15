/// <reference types="bun" />
import { stat } from 'node:fs/promises'
import { resolve, sep } from 'node:path'
import type { ProviderAllowanceSnapshot, ProviderAllowanceTarget } from '../src/lib/types/provider-allowance'

// 临时原型：在真实额度页演示两家重置权益；所有状态仅存内存，不调用真实上游。
// 先构建 WebUI，再运行：bun scripts/mock-allowances-prototype.ts
const root = resolve(import.meta.dir, '..', 'dist')
const port = Number(process.env.MOCK_ALLOWANCES_PORT ?? 4182)
const host = process.env.MOCK_ALLOWANCES_HOST ?? '127.0.0.1'
const DAY = 86_400_000
const reads = new Map<string, { grantId: string; outcome: string }>()
const requests: { at: string; provider: string; grant: string; outcome: string }[] = []
let snapshots: ProviderAllowanceSnapshot[] = []

function seed(): void {
  const now = Date.now()
  const base = (id: string, name: string, vendor: string, plan: string): ProviderAllowanceSnapshot => ({
    provider_id: id,
    provider_name: name,
    catalog_provider_id: vendor,
    channel: vendor === 'openai-codex' ? 'codex' : 'oauth',
    guard_supported: true,
    missing_guarded_keys: [],
    suspension: null,
    plan_label: plan,
    account_label: `${id}@example.invalid`,
    status: 'fresh',
    fetched_at: new Date(now).toISOString(),
    allowances: [],
    models: [],
  })
  const quota = (key: string, label: string, used: number, hours: number) => ({
    key,
    label,
    kind: 'quota_window' as const,
    guarded: true,
    used_percent: used,
    window_seconds: hours * 3600,
    reset_at: now + hours * 3600_000,
    condition: used >= 100 ? ('exhausted' as const) : used >= 80 ? ('tight' as const) : ('normal' as const),
    forecast: { status: 'unknown' as const },
  })
  const codex = base('mock-codex', 'Codex Plus', 'openai-codex', 'Plus')
  codex.allowances = [quota('5h', '5-hour window', 100, 2), quota('7d', 'Weekly window', 96, 48)]
  codex.suspension = {
    suspended_at: new Date(now).toISOString(),
    triggered_keys: ['5h'],
    earliest_reset_at: now + 2 * 3600_000,
  }
  codex.resets = {
    remaining: 2,
    grants: [
      { id: 'codex-soon', label: '活动权益', remaining: 1, expires_at: now + 3 * DAY, usable: true },
      { id: 'codex-month', label: '订阅权益', remaining: 1, expires_at: now + 30 * DAY, usable: true },
    ],
  }
  const claude = base('mock-claude', 'Claude Code Max', 'claude-code', 'Max 5x')
  claude.allowances = [quota('5h', '5-hour window', 100, 4), quota('7d', 'Weekly window', 88, 72)]
  claude.resets = {
    remaining: 3,
    grants: [
      { id: 'claude-current', label: '订阅权益', remaining: 2, expires_at: now + 90 * DAY, usable: true },
      {
        id: 'claude-cooldown',
        label: '活动权益',
        remaining: 1,
        expires_at: now + 5 * DAY,
        usable: false,
        unavailable_reason: 'cooldown',
      },
      {
        id: 'claude-expired',
        label: '活动权益',
        remaining: 1,
        expires_at: now - DAY,
        usable: false,
        unavailable_reason: 'expired',
      },
    ],
  }
  const empty = base('mock-empty', 'Claude Code Pro', 'claude-code', 'Pro')
  empty.allowances = [quota('5h', '5-hour window', 54, 3), quota('7d', 'Weekly window', 69, 100)]
  empty.resets = {
    remaining: 0,
    grants: [
      {
        id: 'empty-grant',
        label: '本期重置权益',
        remaining: 0,
        expires_at: now + 10 * DAY,
        usable: false,
        unavailable_reason: 'exhausted',
      },
    ],
  }
  const unknown = base('mock-unknown', 'Codex Pro', 'openai-codex', 'Pro')
  unknown.allowances = [quota('5h', '5-hour window', 62, 2)]
  unknown.status = 'stale'
  unknown.error = { category: 'upstream_unavailable', message: 'Upstream allowance service is unavailable.' }
  snapshots = [codex, claude, empty, unknown]
  reads.clear()
  requests.length = 0
}
seed()

const json = (data: unknown, status = 200) => Response.json(data, { status })
const target = (snapshot: ProviderAllowanceSnapshot): ProviderAllowanceTarget => ({
  provider_id: snapshot.provider_id,
  provider_name: snapshot.provider_name,
  catalog_provider_id: snapshot.catalog_provider_id,
  channel: snapshot.channel,
  guard_supported: snapshot.guard_supported,
  suspension: snapshot.suspension,
  snapshot,
  refreshing: false,
})
const error = (message: string, status = 400) => json({ error: message, code: 'MOCK_REQUEST_ERROR' }, status)

const annotation = `<style>
#allowance-mock-notice{position:fixed;right:12px;bottom:12px;z-index:100;display:flex;align-items:center;gap:12px;max-width:calc(100vw - 24px);padding:4px 12px;border:1px solid var(--border);border-radius:8px;background:var(--popover);color:var(--foreground);box-shadow:0 4px 12px #0001;font:inherit;font-size:12px;line-height:1.5}
#allowance-mock-notice button{min-height:40px;padding:0 8px;border:0;border-radius:4px;background:none;color:var(--primary);font:inherit;cursor:pointer}#allowance-mock-notice button:hover{background:var(--accent)}
</style><aside id="allowance-mock-notice" aria-label="模拟演示，不连接真实账号"><strong>模拟数据</strong><button type="button" id="allowance-mock-restore">恢复示例</button></aside><script>
document.getElementById('allowance-mock-restore').onclick=async()=>{await fetch('/__mock/reset',{method:'POST'});for(const key of Object.keys(sessionStorage)){if(key.startsWith('provider-allowance-reset:'))sessionStorage.removeItem(key)}location.reload()};
</script>`

Bun.serve({
  hostname: host,
  port,
  async fetch(request) {
    const url = new URL(request.url)
    const path = decodeURIComponent(url.pathname)
    if (path === '/__mock/state') return json({ mode: 'MOCK — memory only, no upstream access', snapshots, requests })
    if (path === '/__mock/reset' && request.method === 'POST') {
      seed()
      return json({ ok: true })
    }
    if (path.startsWith('/api/v1/')) {
      const api = path.slice('/api/v1'.length)
      if (api === '/startup') return json({ status: 'ready', progress: null })
      if (api === '/auth/state')
        return json({ mode: 'server', authenticated: true, setup_authorized: false, username: 'mock-preview' })
      if (api === '/status') return json({ data: { status: 'running' } })
      if (api === '/providers')
        return json({
          data: snapshots.map((s) => ({ id: s.provider_id, name: s.provider_name, credential_status: 'ok' })),
        })
      if (api === '/provider-allowances') return json({ data: snapshots.map(target) })
      const match = /^\/provider-allowances\/([^/]+)(?:\/(refresh|reset|guards))?$/.exec(api)
      if (match) {
        const snapshot = snapshots.find((s) => s.provider_id === match[1])
        if (!snapshot) return error('Mock provider not found', 404)
        if (match[2] === 'reset' && request.method === 'POST') {
          const body = (await request.json()) as { grant_id: string; request_id: string }
          if (!body.grant_id || !body.request_id) return error('grant_id and request_id are required')
          const key = `${snapshot.provider_id}:${body.request_id}`
          const prior = reads.get(key)
          if (prior)
            return prior.grantId === body.grant_id
              ? json({ data: { outcome: prior.outcome } })
              : error('Request ID belongs to another grant', 409)
          const grant = snapshot.resets?.grants.find((g) => g.id === body.grant_id)
          if (!grant) return error('Mock grant not found', 404)
          const outcome =
            grant.usable && grant.remaining > 0 && (grant.expires_at == null || grant.expires_at > Date.now())
              ? 'reset'
              : grant.unavailable_reason === 'cooldown'
                ? 'cooldown'
                : 'unavailable'
          if (outcome === 'reset') {
            grant.remaining--
            snapshot.resets!.remaining--
            if (grant.remaining === 0) {
              grant.usable = false
              grant.unavailable_reason = 'exhausted'
            }
            for (const item of snapshot.allowances) {
              item.used_percent = 0
              item.condition = 'normal'
            }
            snapshot.suspension = null
            snapshot.fetched_at = new Date().toISOString()
          }
          reads.set(key, { grantId: body.grant_id, outcome })
          requests.push({ at: new Date().toISOString(), provider: snapshot.provider_id, grant: grant.id, outcome })
          return json({ data: { outcome } })
        }
        if (match[2] === 'guards' && request.method === 'PUT') {
          const body = (await request.json()) as { keys: string[] }
          for (const item of snapshot.allowances) item.guarded = body.keys.includes(item.key)
          return json({ data: { keys: body.keys, target: target(snapshot) } })
        }
        if (match[2] === 'refresh') snapshot.fetched_at = new Date().toISOString()
        return json({ data: snapshot })
      }
      if (api.startsWith('/settings/')) return json({ data: request.method === 'GET' ? '' : null })
      if (api === '/currency') return json({ data: { custom: null } })
      if (api === '/updates' || api === '/updates/check')
        return json({
          data: {
            current_version: '0.7.2',
            check_status: 'up-to-date',
            last_success_at: null,
            last_failure: null,
            available_update: null,
            skipped: false,
            download_supported: false,
          },
        })
      if (api === '/plugin-marketplace') return json({ data: { sources: [], entries: [], update_count: 0 } })
      if (api === '/api-keys' || api === '/models' || api === '/vendors') return json({ data: [] })
      return error('Not available in the isolated mock', 404)
    }
    if (path.startsWith('/api/') || path.startsWith('/v1')) return error('No upstream access in this mock', 404)
    const file = resolve(root, `.${path}`)
    if (file.startsWith(root + sep)) {
      const info = await stat(file).catch(() => null)
      if (info?.isFile() && !file.endsWith('/index.html')) return new Response(Bun.file(file))
    }
    const html = await Bun.file(resolve(root, 'index.html')).text()
    const locale = `<script>localStorage.setItem('stravia-locale','zh-CN')</script>`
    return new Response(html.replace('<head>', `<head>${locale}`).replace('</body>', `${annotation}</body>`), {
      headers: { 'content-type': 'text/html; charset=utf-8' },
    })
  },
})
console.log(`Quota mock preview: http://${host}:${port}/allowances`)
