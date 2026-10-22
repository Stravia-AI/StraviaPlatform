<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { admin } from '#lib/admin-client.js'
import { formatLogTime } from '#lib/format.js'
import { localeState } from '#lib/localization.svelte.js'
import { modelChangeReason, sourceLabel, summaryDescription } from '#lib/model-changes.js'
import type { Provider, ProviderOAuthStatus } from '#lib/types.js'

interface Props {
  provider: Provider
  /** 渠道使用 OAuth 账号授权。 */
  oauth: boolean
  /** 渠道支持模型发现，才有同步状态可显示。 */
  discovery: boolean
}

interface StatusItem {
  key: string
  label: string
  value: string
  notes: Array<{ text: string; tone?: 'destructive' }>
}

let { provider, oauth, discovery }: Props = $props()

const syncStateQuery = createQuery(() => ({
  queryKey: ['provider-model-sync-state', provider.id],
  queryFn: () => admin.providers.syncState(provider.id),
  enabled: discovery,
}))

const oauthLabels: Record<ProviderOAuthStatus, () => string> = {
  not_connected: m.provider_connection_oauth_not_connected,
  pending: m.provider_connection_oauth_pending,
  connected: m.provider_connection_oauth_connected,
  unavailable: m.provider_connection_oauth_unavailable,
  quota_exhausted: m.provider_connection_oauth_quota_exhausted,
  error: m.provider_connection_oauth_error,
  disconnected: m.provider_connection_oauth_disconnected,
}

// 只收集已有的真实状态；没有任何内容时整栏不渲染。
const items = $derived.by<StatusItem[]>(() => {
  const locale = localeState.current
  const result: StatusItem[] = []
  if (provider.credential_status === 'invalid') {
    result.push({
      key: 'credential',
      label: m.provider_connection_status_credential(),
      value: provider.credential_invalid_at
        ? m.provider_connection_status_credential_invalid_since({
            time: formatLogTime(provider.credential_invalid_at, locale),
          })
        : m.providers_credential_invalid(),
      notes: [],
    })
  }
  if (oauth && provider.oauth_status) {
    const status = oauthLabels[provider.oauth_status]?.() ?? provider.oauth_status
    result.push({
      key: 'account',
      label: m.provider_connection_status_account(),
      value: provider.oauth_expires_at
        ? m.provider_connection_status_account_expires({
            status,
            time: formatLogTime(provider.oauth_expires_at, locale),
          })
        : status,
      notes: provider.oauth_last_error ? [{ text: provider.oauth_last_error, tone: 'destructive' }] : [],
    })
  }
  const suspension = provider.allowance_suspension
  if (suspension) {
    result.push({
      key: 'allowance',
      label: m.provider_connection_status_allowance(),
      value: m.allowances_suspended_since({ time: formatLogTime(suspension.suspended_at, locale) }),
      notes: [
        { text: m.allowances_suspension_triggers({ keys: suspension.triggered_keys.join(', ') }) },
        ...(suspension.earliest_reset_at != null
          ? [{ text: m.allowances_reset_at({ time: formatLogTime(suspension.earliest_reset_at, locale) }) }]
          : []),
      ],
    })
  }
  const check = syncStateQuery.data?.last_check
  if (check) {
    const description = summaryDescription(check)
    result.push({
      key: 'sync',
      label: m.provider_connection_status_sync(),
      value: `${formatLogTime(check.checked_at, locale)} · ${sourceLabel(check.source)}`,
      notes: [
        ...(description ? [{ text: description }] : []),
        ...(check.error ? [{ text: modelChangeReason(check.error), tone: 'destructive' as const }] : []),
      ],
    })
  }
  return result
})
</script>

{#if items.length > 0}
  <aside
    aria-label={m.provider_connection_status_title()}
    class="min-w-0 border-t pt-4 @min-[80rem]/route-page:border-t-0 @min-[80rem]/route-page:pt-0">
    <dl class="flex flex-col gap-4 text-sm">
      {#each items as item (item.key)}
        <div class="min-w-0">
          <dt class="text-muted-foreground">{item.label}</dt>
          <dd class="mt-0.5 break-words">{item.value}</dd>
          {#each item.notes as note (note.text)}
            <dd
              class={[
                'mt-0.5 break-words',
                note.tone === 'destructive' ? 'text-destructive' : 'text-muted-foreground',
              ]}>
              {note.text}
            </dd>
          {/each}
        </div>
      {/each}
    </dl>
  </aside>
{/if}
