<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import GaugeIcon from '@lucide/svelte/icons/gauge'
import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import { SvelteSet } from 'svelte/reactivity'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatAllowanceAmount, formatAllowancePercent } from '$lib/provider-allowance-format'
import { formatLogTime } from '$lib/format'
import { localeState } from '$lib/localization.svelte'
import type {
  Allowance,
  ModelAllowance,
  ProviderAllowanceErrorCategory,
  ProviderAllowanceSnapshot,
  ProviderAllowanceStatus,
} from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import { Badge, type BadgeVariant } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Card from '$lib/components/ui/card'
import { Skeleton } from '$lib/components/ui/skeleton'

const queryClient = useQueryClient()
const allowanceQuery = createQuery(() => ({
  queryKey: ['provider-allowances'],
  queryFn: admin.allowances.list,
  refetchInterval: 180_000,
}))

let refreshingAll = $state(false)
const refreshingProviderIds = new SvelteSet<string>()
const collator = $derived(new Intl.Collator(localeState.current, { sensitivity: 'base', numeric: true }))
const snapshots = $derived.by(() =>
  [...(allowanceQuery.data ?? [])].sort(
    (left, right) =>
      collator.compare(left.provider_name, right.provider_name) || left.provider_id.localeCompare(right.provider_id),
  ),
)

async function refreshAll(): Promise<void> {
  refreshingAll = true
  try {
    await admin.allowances.refreshAll()
    await queryClient.invalidateQueries({ queryKey: ['provider-allowances'] })
    toast.success(m.allowances_refreshed_all())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    refreshingAll = false
  }
}

async function refreshProvider(snapshot: ProviderAllowanceSnapshot): Promise<void> {
  refreshingProviderIds.add(snapshot.provider_id)
  try {
    await admin.allowances.refresh(snapshot.provider_id)
    await queryClient.invalidateQueries({ queryKey: ['provider-allowances'] })
    toast.success(m.allowances_refreshed_provider({ provider: snapshot.provider_name }))
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    refreshingProviderIds.delete(snapshot.provider_id)
  }
}

function statusPresentation(status: ProviderAllowanceStatus): { label: string; variant: BadgeVariant } {
  switch (status) {
    case 'fresh':
      return { label: m.allowances_status_fresh(), variant: 'secondary' }
    case 'stale':
      return { label: m.allowances_status_stale(), variant: 'outline' }
    case 'error':
      return { label: m.allowances_status_error(), variant: 'destructive' }
  }
}

function allowanceLabel(allowance: Allowance): string {
  switch (allowance.key) {
    case '5h':
      return m.allowances_label_five_hour()
    case '7d':
    case 'weekly':
      return m.allowances_label_weekly()
    case 'daily':
      return m.allowances_label_daily()
    case 'monthly':
      return m.allowances_label_monthly()
    case 'billing_cycle':
      return m.allowances_label_billing_cycle()
    case 'credits':
    case 'credits_balance':
      return m.allowances_label_credit_balance()
    case 'credits_unlimited':
      return m.allowances_label_unlimited_credits()
    case 'premium_interactions':
      return m.allowances_label_premium_interactions()
    case 'mcp_tools':
      return m.allowances_label_mcp_tools()
    case 'extra_usage':
      return m.allowances_label_extra_usage()
    case 'tokens':
      return m.allowances_label_tokens()
    default:
      return allowance.label
  }
}

function allowanceErrorMessage(category: ProviderAllowanceErrorCategory): string {
  switch (category) {
    case 'authentication':
      return m.allowances_error_authentication()
    case 'rate_limited':
      return m.allowances_error_rate_limited()
    case 'timeout':
      return m.allowances_error_timeout()
    case 'upstream_unavailable':
      return m.allowances_error_upstream_unavailable()
    case 'invalid_response':
      return m.allowances_error_invalid_response()
  }
}
</script>

<svelte:head><title>{m.allowances_title()} · Stravia</title></svelte:head>

{#snippet allowanceRows(allowances: Allowance[])}
  <div class="grid gap-3">
    {#each allowances as allowance (allowance.key)}
      {@const label = allowanceLabel(allowance)}
      {@const usedPercent = allowance.used_percent}
      {@const progressPercent = usedPercent == null ? undefined : Math.min(100, Math.max(0, usedPercent))}
      {@const remainingPercent = usedPercent == null ? undefined : Math.max(0, 100 - usedPercent)}
      <section class="rounded-lg border bg-muted/20 p-3" aria-label={label}>
        <div class="flex flex-wrap items-baseline justify-between gap-2">
          <h4 class="text-sm font-medium">{label}</h4>
        </div>
        {#if allowance.used || allowance.remaining || allowance.limit}
          <dl class="mt-3 grid grid-cols-2 gap-x-4 gap-y-3 text-sm sm:grid-cols-4">
            {#if allowance.used}
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.allowances_used()}</dt>
                <dd class="font-technical mt-1 break-words font-medium tabular-nums">
                  {formatAllowanceAmount(allowance.used, localeState.current)}
                </dd>
              </div>
            {/if}
            {#if allowance.remaining}
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.allowances_remaining()}</dt>
                <dd class="font-technical mt-1 break-words font-medium tabular-nums">
                  {formatAllowanceAmount(allowance.remaining, localeState.current)}
                </dd>
              </div>
            {/if}
            {#if allowance.limit}
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.allowances_limit()}</dt>
                <dd class="font-technical mt-1 break-words font-medium tabular-nums">
                  {formatAllowanceAmount(allowance.limit, localeState.current)}
                </dd>
              </div>
            {/if}
          </dl>
        {/if}
        {#if progressPercent != null}
          <div class="mt-3 flex items-center justify-between gap-3 text-xs text-muted-foreground">
            <span>
              {m.allowances_used()}
              <span class="font-technical ml-1 font-medium text-foreground tabular-nums">
                {formatAllowancePercent(usedPercent, localeState.current)}
              </span>
            </span>
            <span>
              {m.allowances_remaining()}
              <span class="font-technical ml-1 font-medium text-foreground tabular-nums">
                {formatAllowancePercent(remainingPercent, localeState.current)}
              </span>
            </span>
          </div>
          <div
            class="mt-3 h-1.5 overflow-hidden rounded-full bg-muted"
            role="progressbar"
            aria-label={`${label} ${m.allowances_utilization()}`}
            aria-valuemin="0"
            aria-valuemax="100"
            aria-valuenow={progressPercent}>
            <div class="h-full rounded-full bg-primary" style:width={`${progressPercent}%`}></div>
          </div>
        {/if}
        {#if allowance.reset_at != null}
          <p class="font-technical mt-3 text-xs text-muted-foreground tabular-nums">
            {m.allowances_reset_at({ time: formatLogTime(allowance.reset_at, localeState.current) })}
          </p>
        {/if}
      </section>
    {/each}
  </div>
{/snippet}

{#snippet modelRows(models: ModelAllowance[])}
  <div class="mt-4 grid gap-3">
    {#each models as model (model.model)}
      <details class="group rounded-lg border bg-background">
        <summary
          class="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3 py-2 text-sm font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
          <span class="min-w-0 break-all">{model.model}</span>
          <ChevronDownIcon class="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-180" />
        </summary>
        <div class="border-t p-3">{@render allowanceRows(model.allowances)}</div>
      </details>
    {/each}
  </div>
{/snippet}

<div class="route-page">
  <PageHeader eyebrow={m.common_monitor()} title={m.allowances_title()} description={m.allowances_page_summary()}>
    {#snippet actions()}
      <Button onclick={refreshAll} disabled={refreshingAll || allowanceQuery.isPending}>
        {#if refreshingAll}<LoaderCircleIcon class="animate-spin" />{:else}<RefreshCwIcon />{/if}
        {m.allowances_refresh_all()}
      </Button>
    {/snippet}
  </PageHeader>

  {#if allowanceQuery.isPending}
    <div class="grid gap-5 lg:grid-cols-2" aria-label={m.allowances_loading()}>
      {#each Array(4) as _, index (index)}
        <Card.Root
          ><Card.Header><Skeleton class="h-6 w-40" /><Skeleton class="h-4 w-56" /></Card.Header><Card.Content>
            <Skeleton class="h-28 w-full" />
          </Card.Content></Card.Root>
      {/each}
    </div>
  {:else if allowanceQuery.error && allowanceQuery.data === undefined}
    <section class="route-section py-12 text-center" role="alert">
      <GaugeIcon class="mx-auto size-8 text-destructive" />
      <h2 class="route-section-title mt-4">{m.allowances_load_failed()}</h2>
      <p class="mt-2 text-sm text-destructive">{localizeBackendErrorMessage(allowanceQuery.error)}</p>
      <Button class="mt-4" variant="outline" onclick={() => void allowanceQuery.refetch()}>{m.common_retry()}</Button>
    </section>
  {:else if snapshots.length === 0}
    <section class="route-section py-12 text-center">
      <GaugeIcon class="mx-auto size-8 text-muted-foreground" />
      <h2 class="route-section-title mt-4">{m.allowances_empty_title()}</h2>
      <p class="route-section-description mx-auto max-w-lg">{m.allowances_empty_description()}</p>
      <Button class="mt-4" variant="outline" href="/providers">{m.allowances_manage_providers()}</Button>
    </section>
  {:else}
    <div class="grid min-w-0 gap-5 lg:grid-cols-2">
      {#each snapshots as snapshot (snapshot.provider_id)}
        {@const refreshingProvider = refreshingProviderIds.has(snapshot.provider_id)}
        {@const presentation = statusPresentation(snapshot.status)}
        <Card.Root class="min-w-0 overflow-hidden" data-testid={`allowance-card-${snapshot.status}`}>
          <Card.Header class="border-b">
            <div class="min-w-0">
              <div class="flex flex-wrap items-center gap-2">
                <Card.Title class="break-words">{snapshot.provider_name}</Card.Title>
                <Badge variant={presentation.variant}>{presentation.label}</Badge>
              </div>
              <Card.Description class="font-technical mt-1 break-all text-xs">
                {snapshot.catalog_provider_id} / {snapshot.channel}
              </Card.Description>
              {#if snapshot.plan_label}
                <p class="mt-2 text-sm text-muted-foreground">{m.allowances_plan({ plan: snapshot.plan_label })}</p>
              {/if}
            </div>
            <Card.Action>
              <Button
                size="sm"
                variant="outline"
                onclick={() => refreshProvider(snapshot)}
                disabled={refreshingProvider || refreshingAll}
                aria-label={m.allowances_refresh_provider({ provider: snapshot.provider_name })}>
                {#if refreshingProvider}<LoaderCircleIcon class="animate-spin" />{:else}<RefreshCwIcon />{/if}
              </Button>
            </Card.Action>
          </Card.Header>
          <Card.Content class="pt-5">
            <div class="flex flex-wrap justify-between gap-2 text-xs text-muted-foreground">
              <span class="font-technical tabular-nums">
                {snapshot.fetched_at
                  ? m.allowances_last_updated({ time: formatLogTime(snapshot.fetched_at, localeState.current) })
                  : m.allowances_never_updated()}
              </span>
            </div>

            {#if snapshot.error}
              <div
                class={[
                  'mt-4 rounded-lg border px-3 py-2 text-sm',
                  snapshot.status === 'stale'
                    ? 'border-amber-500/35 bg-amber-500/8 text-foreground'
                    : 'border-destructive/30 bg-destructive/5 text-destructive',
                ]}
                role="status">
                {#if snapshot.status === 'stale'}<p class="font-medium">{m.allowances_stale_message()}</p>{/if}
                <p class={snapshot.status === 'stale' ? 'mt-1 text-muted-foreground' : undefined}>
                  {allowanceErrorMessage(snapshot.error.category)}
                </p>
                <div class="mt-3 flex flex-wrap gap-2">
                  <Button
                    size="sm"
                    variant="outline"
                    onclick={() => refreshProvider(snapshot)}
                    disabled={refreshingProvider || refreshingAll}>
                    {#if refreshingProvider}<LoaderCircleIcon class="animate-spin" />{:else}<RefreshCwIcon />{/if}
                    {m.common_retry()}
                  </Button>
                  {#if snapshot.error.category === 'authentication'}
                    <Button
                      size="sm"
                      variant="outline"
                      href={`/providers/${encodeURIComponent(snapshot.provider_id)}?view=connection`}>
                      {m.allowances_open_provider()}
                    </Button>
                  {/if}
                </div>
              </div>
            {/if}

            {#if snapshot.allowances.length > 0}
              <div class="mt-4">{@render allowanceRows(snapshot.allowances)}</div>
            {:else if snapshot.models.length === 0 && !snapshot.error}
              <p class="mt-4 rounded-lg border py-6 text-center text-sm text-muted-foreground">
                {m.allowances_no_values()}
              </p>
            {/if}

            {#if snapshot.models.length > 0}
              <div class="mt-5 border-t pt-4">
                <h3 class="text-sm font-semibold">{m.allowances_model_allowances()}</h3>
                {@render modelRows(snapshot.models)}
              </div>
            {/if}
          </Card.Content>
        </Card.Root>
      {/each}
    </div>
  {/if}
</div>
