<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import Clock3Icon from '@lucide/svelte/icons/clock-3'
import GaugeIcon from '@lucide/svelte/icons/gauge'
import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SearchIcon from '@lucide/svelte/icons/search'
import TrendingDownIcon from '@lucide/svelte/icons/trending-down'
import { SvelteMap, SvelteSet } from 'svelte/reactivity'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import { formatLogTime } from '$lib/format'
import { localeState } from '$lib/localization.svelte'
import { formatAllowanceAmount, formatAllowancePercent } from '$lib/provider-allowance-format'
import type {
  Allowance,
  AllowanceCondition,
  ModelAllowance,
  ProviderAllowanceErrorCategory,
  ProviderAllowanceSnapshot,
  ProviderAllowanceStatus,
} from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import { Badge, type BadgeVariant } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Card from '$lib/components/ui/card'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRow,
} from '$lib/components/ui/data-table'
import * as InputGroup from '$lib/components/ui/input-group'
import * as Select from '$lib/components/ui/select'
import { Skeleton } from '$lib/components/ui/skeleton'

interface VisibleProvider {
  snapshot: ProviderAllowanceSnapshot
  allowances: Allowance[]
}

interface VisibleAllowance {
  snapshot: ProviderAllowanceSnapshot
  allowance: Allowance
}

interface AllowanceMatrixRow {
  snapshot: ProviderAllowanceSnapshot
  allowance?: Allowance
}

const queryClient = useQueryClient()
const allowanceQuery = createQuery(() => ({
  queryKey: ['provider-allowances'],
  queryFn: admin.allowances.list,
  refetchInterval: 180_000,
}))

let refreshingAll = $state(false)
let searchQuery = $state('')
let catalogFilter = $state('all')
let conditionFilter = $state<'all' | AllowanceCondition>('all')
let freshnessFilter = $state<'all' | ProviderAllowanceStatus>('all')
const refreshingProviderIds = new SvelteSet<string>()
const collator = $derived(new Intl.Collator(localeState.current, { sensitivity: 'base', numeric: true }))
const snapshots = $derived.by(() =>
  [...(allowanceQuery.data ?? [])].sort(
    (left, right) =>
      collator.compare(left.provider_name, right.provider_name) || left.provider_id.localeCompare(right.provider_id),
  ),
)
const catalogOptions = $derived.by(() => {
  const options = new SvelteMap<string, string>()
  for (const snapshot of snapshots) {
    const value = catalogValue(snapshot)
    options.set(value, `${snapshot.catalog_provider_id} / ${snapshot.channel}`)
  }
  return [...options].map(([value, label]) => ({ value, label })).sort((left, right) => collator.compare(left.label, right.label))
})
const catalogFilterLabel = $derived(
  catalogFilter === 'all'
    ? m.allowances_filter_all()
    : (catalogOptions.find((option) => option.value === catalogFilter)?.label ?? m.allowances_filter_all()),
)
const conditionFilterLabel = $derived(
  conditionFilter === 'all' ? m.allowances_filter_all() : conditionLabel(conditionFilter),
)
const freshnessFilterLabel = $derived(
  freshnessFilter === 'all' ? m.allowances_filter_all() : statusPresentation(freshnessFilter).label,
)
const visibleProviders = $derived.by((): VisibleProvider[] => {
  const query = searchQuery.trim().toLocaleLowerCase(localeState.current)
  return snapshots.flatMap((snapshot) => {
    if (query && !snapshot.provider_name.toLocaleLowerCase(localeState.current).includes(query)) return []
    if (catalogFilter !== 'all' && catalogValue(snapshot) !== catalogFilter) return []
    if (freshnessFilter !== 'all' && snapshot.status !== freshnessFilter) return []
    const allowances =
      conditionFilter === 'all'
        ? snapshot.allowances
        : snapshot.allowances.filter((allowance) => effectiveCondition(allowance) === conditionFilter)
    if (conditionFilter !== 'all' && allowances.length === 0) return []
    return [{ snapshot, allowances }]
  })
})
const visibleAllowances = $derived(
  visibleProviders.flatMap(({ snapshot, allowances }) => allowances.map((allowance) => ({ snapshot, allowance }))),
)
const allowanceMatrixRows = $derived(
  visibleProviders.flatMap(({ snapshot, allowances }) =>
    allowances.length > 0 ? allowances.map((allowance) => ({ snapshot, allowance })) : [{ snapshot }],
  ),
)
const tableLabels = $derived(getDataTableLabels())
const allowanceColumnHelper = createDataTableColumnHelper<AllowanceMatrixRow>()
const allowanceColumns = allowanceColumnHelper.columns([
  allowanceColumnHelper.accessor(({ snapshot }) => snapshot.provider_id, {
    id: 'provider',
    header: '',
    enableSorting: false,
  }),
  allowanceColumnHelper.display({
    id: 'allowance',
    header: () => m.allowances_item(),
    cell: (context) => renderSnippet(allowanceItemCell, context),
    enableSorting: false,
    meta: { label: () => m.allowances_item(), headerClass: 'w-[14rem]' },
  }),
  allowanceColumnHelper.display({
    id: 'used',
    header: () => m.allowances_used(),
    cell: (context) => renderSnippet(allowanceUsedCell, context),
    enableSorting: false,
    meta: { label: () => m.allowances_used(), headerClass: 'w-[8rem]' },
  }),
  allowanceColumnHelper.display({
    id: 'remaining',
    header: () => m.allowances_remaining(),
    cell: (context) => renderSnippet(allowanceRemainingCell, context),
    enableSorting: false,
    meta: { label: () => m.allowances_remaining(), headerClass: 'w-[9rem]' },
  }),
  allowanceColumnHelper.display({
    id: 'reset',
    header: () => m.allowances_reset(),
    cell: (context) => renderSnippet(allowanceResetCell, context),
    enableSorting: false,
    meta: { label: () => m.allowances_reset(), headerClass: 'w-[11rem]' },
  }),
])
const allowanceGrouping = ['provider']
const allowanceColumnVisibility = { provider: false }
const overallCondition = $derived(worstCondition(visibleAllowances.map(({ allowance }) => effectiveCondition(allowance))))
const lowestRemaining = $derived.by(() => {
  const values = visibleAllowances
    .map(({ allowance }) => remainingPercent(allowance))
    .filter((value): value is number => value != null && Number.isFinite(value))
  return values.length ? Math.min(...values) : undefined
})
const timeline = $derived.by(() =>
  visibleAllowances
    .filter((item): item is VisibleAllowance & { allowance: Allowance & { reset_at: number } } => item.allowance.reset_at != null)
    .sort(
      (left, right) =>
        left.allowance.reset_at - right.allowance.reset_at ||
        collator.compare(left.snapshot.provider_name, right.snapshot.provider_name) ||
        collator.compare(allowanceLabel(left.allowance), allowanceLabel(right.allowance)),
    ),
)
const forecastSummary = $derived.by(() => {
  let noRisk = 0
  let willExhaust = 0
  let unknown = 0
  const projected: number[] = []
  const risks: VisibleAllowance[] = []
  for (const item of timeline) {
    if (item.allowance.forecast.projected_remaining_percent != null) {
      projected.push(item.allowance.forecast.projected_remaining_percent)
    }
    switch (item.allowance.forecast.status) {
      case 'no_risk':
        noRisk += 1
        break
      case 'will_exhaust':
        willExhaust += 1
        risks.push(item)
        break
      case 'unknown':
        unknown += 1
        break
    }
  }
  return {
    noRisk,
    willExhaust,
    unknown,
    lowestProjected: projected.length ? Math.min(...projected) : undefined,
    risks,
  }
})
const latestFetchedAt = $derived.by(() => {
  const timestamps = snapshots
    .map((snapshot) => snapshot.fetched_at)
    .filter((value): value is string => Boolean(value))
    .sort()
  return timestamps.at(-1)
})

function catalogValue(snapshot: ProviderAllowanceSnapshot): string {
  return `${snapshot.catalog_provider_id}::${snapshot.channel}`
}

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

function conditionLabel(condition: AllowanceCondition | undefined): string {
  switch (condition) {
    case 'normal':
      return m.allowances_condition_normal()
    case 'tight':
      return m.allowances_condition_tight()
    case 'exhausted':
      return m.allowances_condition_exhausted()
    default:
      return m.allowances_condition_unknown()
  }
}

function conditionVariant(condition: AllowanceCondition | undefined): BadgeVariant {
  return condition === 'exhausted' ? 'destructive' : condition === 'tight' ? 'outline' : 'secondary'
}

function conditionTone(condition: AllowanceCondition | undefined): string {
  switch (condition) {
    case 'exhausted':
      return 'border-red-500/35 bg-red-500/8'
    case 'tight':
      return 'border-amber-500/35 bg-amber-500/8'
    case 'normal':
      return 'border-emerald-500/35 bg-emerald-500/8'
    default:
      return 'border-border bg-muted/30'
  }
}

function effectiveCondition(allowance: Allowance): AllowanceCondition | undefined {
  return allowance.condition === 'exhausted' && allowance.reset_at == null ? undefined : allowance.condition
}

function worstCondition(conditions: (AllowanceCondition | undefined)[]): AllowanceCondition | undefined {
  let result: AllowanceCondition | undefined
  const rank = { normal: 1, tight: 2, exhausted: 3 } satisfies Record<AllowanceCondition, number>
  for (const condition of conditions) {
    if (condition && (!result || rank[condition] > rank[result])) result = condition
  }
  return result
}

function remainingPercent(allowance: Allowance): number | undefined {
  if (allowance.used_percent != null && Number.isFinite(allowance.used_percent)) {
    return Math.max(0, 100 - allowance.used_percent)
  }
  if (allowance.remaining && allowance.limit && Number.isFinite(allowance.remaining.value) && allowance.limit.value > 0) {
    return Math.max(0, (allowance.remaining.value / allowance.limit.value) * 100)
  }
  return undefined
}

function usedDisplay(allowance: Allowance): string {
  return allowance.used_percent != null
    ? formatAllowancePercent(allowance.used_percent, localeState.current)
    : formatAllowanceAmount(allowance.used, localeState.current)
}

function remainingDisplay(allowance: Allowance): string {
  const percent = remainingPercent(allowance)
  return percent != null
    ? formatAllowancePercent(percent, localeState.current)
    : formatAllowanceAmount(allowance.remaining, localeState.current)
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

function allowanceRowId(item: AllowanceMatrixRow): string {
  return item.allowance
    ? `${item.snapshot.provider_id}:${item.allowance.key}`
    : `${item.snapshot.provider_id}:empty`
}

function allowanceRowClass(row: DataTableRow<AllowanceMatrixRow>): string {
  if (row.getIsGrouped()) return 'bg-muted/35 hover:bg-muted/35'
  return row.original.allowance ? 'hover:bg-muted/20' : 'hidden'
}
</script>

<svelte:head><title>{m.allowances_title()} · Stravia</title></svelte:head>

{#snippet allowanceProviderSummary(snapshot: ProviderAllowanceSnapshot, allowances: Allowance[])}
  {@const presentation = statusPresentation(snapshot.status)}
  {@const providerCondition = worstCondition(allowances.map(effectiveCondition))}
  {@const refreshingProvider = refreshingProviderIds.has(snapshot.provider_id)}
  <div class="min-w-0">
    <div class="flex flex-wrap items-center gap-2">
      <h3 class="font-semibold">{snapshot.provider_name}</h3>
      <Badge variant={presentation.variant}>{presentation.label}</Badge>
      {#if providerCondition}<Badge variant={conditionVariant(providerCondition)}>{conditionLabel(providerCondition)}</Badge>{/if}
      {#if snapshot.plan_label}<span class="text-xs text-muted-foreground">{snapshot.plan_label}</span>{/if}
    </div>
    <p class="font-technical mt-1 text-xs text-muted-foreground">{snapshot.catalog_provider_id} / {snapshot.channel}</p>
    {#if snapshot.error}
      <p class="mt-1.5 text-xs text-muted-foreground">
        {snapshot.status === 'stale' ? `${m.allowances_stale_message()} ` : ''}{allowanceErrorMessage(snapshot.error.category)}
      </p>
    {/if}
    {#if snapshot.models.length > 0}
      <details class="group mt-1.5">
        <summary
          class="inline-flex cursor-pointer list-none items-center gap-1 text-xs font-medium text-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          aria-label={m.allowances_show_model_allowances({ provider: snapshot.provider_name })}>
          {m.allowances_model_allowances()}
          <ChevronDownIcon class="size-3.5 transition-transform group-open:rotate-180" />
        </summary>
        <div class="mt-3 max-w-2xl">{@render modelRows(snapshot.models)}</div>
      </details>
    {/if}
  </div>
  <Button
    size="icon"
    class="size-10"
    variant="ghost"
    onclick={() => refreshProvider(snapshot)}
    disabled={refreshingProvider || refreshingAll}
    aria-label={m.allowances_refresh_provider({ provider: snapshot.provider_name })}>
    {#if refreshingProvider}<LoaderCircleIcon class="animate-spin" />{:else}<RefreshCwIcon />{/if}
  </Button>
{/snippet}

{#snippet allowanceGroupRow(row: DataTableRow<AllowanceMatrixRow>)}
  {@const leaves = row.getLeafRows()}
  {@const snapshot = leaves[0]?.original.snapshot}
  {#if snapshot}
    {@const allowances = leaves.flatMap(({ original }) => original.allowance ? [original.allowance] : [])}
    <div
      class="flex min-h-14 items-center justify-between gap-3 px-3 py-2"
      data-testid={`allowance-provider-${snapshot.provider_id}`}>
      {@render allowanceProviderSummary(snapshot, allowances)}
    </div>
  {/if}
{/snippet}

{#snippet allowanceItemCell(context: DataTableCellContext<AllowanceMatrixRow>)}
  {@const allowance = context.row.original.allowance}
  {#if allowance}
    <div class="flex min-w-0 items-center gap-2">
      <span class="size-1.5 shrink-0 rounded-full bg-muted-foreground/50"></span>
      <span class="truncate font-medium">{allowanceLabel(allowance)}</span>
    </div>
  {/if}
{/snippet}

{#snippet allowanceUsedCell(context: DataTableCellContext<AllowanceMatrixRow>)}
  {@const allowance = context.row.original.allowance}
  {#if allowance}
    {@const percent = allowance.used_percent == null ? undefined : Math.min(100, Math.max(0, allowance.used_percent))}
    <span class="font-technical tabular-nums">{usedDisplay(allowance)}</span>
    {#if percent != null}
      <div class="mt-1.5 h-1 overflow-hidden rounded-full bg-muted" role="progressbar" aria-label={`${allowanceLabel(allowance)} ${m.allowances_utilization()}`} aria-valuemin="0" aria-valuemax="100" aria-valuenow={percent}>
        <div class="h-full rounded-full bg-primary" style:width={`${percent}%`}></div>
      </div>
    {/if}
  {/if}
{/snippet}

{#snippet allowanceRemainingCell(context: DataTableCellContext<AllowanceMatrixRow>)}
  {@const allowance = context.row.original.allowance}
  {#if allowance}
    {@const condition = effectiveCondition(allowance)}
    <div class="flex min-w-0 items-center gap-2">
      {#if condition}<span class={['size-1.5 shrink-0 rounded-full', condition === 'exhausted' ? 'bg-red-500' : condition === 'tight' ? 'bg-amber-500' : 'bg-emerald-500']}></span>{/if}
      <span class="font-technical tabular-nums">{remainingDisplay(allowance)}</span>
    </div>
  {/if}
{/snippet}

{#snippet allowanceResetCell(context: DataTableCellContext<AllowanceMatrixRow>)}
  {@const allowance = context.row.original.allowance}
  {#if allowance}
    <span class="font-technical text-muted-foreground tabular-nums">
      {allowance.reset_at != null ? formatLogTime(allowance.reset_at, localeState.current) : '–'}
    </span>
  {/if}
{/snippet}

{#snippet mobileAllowanceRow(allowance: Allowance)}
  {@const condition = effectiveCondition(allowance)}
  {@const percent = allowance.used_percent == null ? undefined : Math.min(100, Math.max(0, allowance.used_percent))}
  <div class="border-t px-3 py-2.5">
    <div class="flex min-w-0 items-start justify-between gap-3">
      <div class="flex min-w-0 items-center gap-2">
        <span class="size-1.5 shrink-0 rounded-full bg-muted-foreground/50"></span>
        <span class="truncate font-medium">{allowanceLabel(allowance)}</span>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        {#if condition}<span class={['size-1.5 rounded-full', condition === 'exhausted' ? 'bg-red-500' : condition === 'tight' ? 'bg-amber-500' : 'bg-emerald-500']}></span>{/if}
        <span class="font-technical tabular-nums">{remainingDisplay(allowance)}</span>
      </div>
    </div>
    <div class="mt-1.5 grid grid-cols-[auto_minmax(0,1fr)] gap-3 text-xs text-muted-foreground">
      <span class="font-technical tabular-nums">{m.allowances_used()} {usedDisplay(allowance)}</span>
      <span class="font-technical truncate text-right tabular-nums">
        {allowance.reset_at != null ? m.allowances_reset_at({ time: formatLogTime(allowance.reset_at, localeState.current) }) : '–'}
      </span>
    </div>
    {#if percent != null}
      <div class="mt-2 h-1 overflow-hidden rounded-full bg-muted" role="progressbar" aria-label={`${allowanceLabel(allowance)} ${m.allowances_utilization()}`} aria-valuemin="0" aria-valuemax="100" aria-valuenow={percent}>
        <div class="h-full rounded-full bg-primary" style:width={`${percent}%`}></div>
      </div>
    {/if}
  </div>
{/snippet}

{#snippet compactAllowanceRows(allowances: Allowance[])}
  <div class="grid gap-2">
    {#each allowances as allowance (allowance.key)}
      <div class="grid gap-2 rounded-md border bg-muted/20 p-3 text-sm sm:grid-cols-[minmax(0,1fr)_auto_auto]">
        <span class="font-medium">{allowanceLabel(allowance)}</span>
        <span class="font-technical tabular-nums">{remainingDisplay(allowance)}</span>
        <span class="font-technical text-muted-foreground tabular-nums">
          {allowance.reset_at != null ? m.allowances_reset_at({ time: formatLogTime(allowance.reset_at, localeState.current) }) : '–'}
        </span>
      </div>
    {/each}
  </div>
{/snippet}

{#snippet modelRows(models: ModelAllowance[])}
  <div class="grid gap-2">
    {#each models as model (model.model)}
      <details class="group rounded-lg border bg-background">
        <summary class="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3 py-2 text-sm font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
          <span class="min-w-0 break-all">{model.model}</span>
          <ChevronDownIcon class="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-180" />
        </summary>
        <div class="border-t p-3">{@render compactAllowanceRows(model.allowances)}</div>
      </details>
    {/each}
  </div>
{/snippet}

<div class="allowances-page route-page">
  <PageHeader eyebrow={m.common_monitor()} title={m.allowances_title()} description={m.allowances_page_summary()}>
    {#snippet actions()}
      <div class="flex flex-wrap items-center justify-end gap-3">
        <span class="font-technical text-xs text-muted-foreground tabular-nums">
          {latestFetchedAt ? m.allowances_last_updated({ time: formatLogTime(latestFetchedAt, localeState.current) }) : m.allowances_never_updated()}
        </span>
        <Button onclick={refreshAll} disabled={refreshingAll || allowanceQuery.isPending}>
          {#if refreshingAll}<LoaderCircleIcon class="animate-spin" />{:else}<RefreshCwIcon />{/if}
          {m.allowances_refresh_all()}
        </Button>
      </div>
    {/snippet}
  </PageHeader>

  {#if allowanceQuery.isPending}
    <div class="grid gap-5 xl:grid-cols-[minmax(0,2fr)_minmax(17rem,1fr)]" aria-label={m.allowances_loading()}>
      <Skeleton class="h-96 w-full" />
      <div class="grid gap-5"><Skeleton class="h-48 w-full" /><Skeleton class="h-56 w-full" /></div>
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
    <section class="route-section grid gap-2 p-2 sm:grid-cols-2 xl:grid-cols-[minmax(14rem,1fr)_repeat(3,minmax(10rem,auto))]">
      <InputGroup.Root class="min-w-0">
        <InputGroup.Input
          type="search"
          aria-label={m.allowances_search_label()}
          placeholder={m.allowances_search_placeholder()}
          bind:value={searchQuery} />
        <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
      </InputGroup.Root>
      <Select.Root type="single" bind:value={catalogFilter}>
        <Select.Trigger class="w-full" aria-label={m.allowances_filter_catalog()}>{catalogFilterLabel}</Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="all">{m.allowances_filter_all()}</Select.Item>
            {#each catalogOptions as option (option.value)}
              <Select.Item value={option.value}>{option.label}</Select.Item>
            {/each}
          </Select.Group>
        </Select.Content>
      </Select.Root>
      <Select.Root type="single" bind:value={conditionFilter}>
        <Select.Trigger class="w-full" aria-label={m.allowances_filter_condition()}>{conditionFilterLabel}</Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="all">{m.allowances_filter_all()}</Select.Item>
            <Select.Item value="normal">{m.allowances_condition_normal()}</Select.Item>
            <Select.Item value="tight">{m.allowances_condition_tight()}</Select.Item>
            <Select.Item value="exhausted">{m.allowances_condition_exhausted()}</Select.Item>
          </Select.Group>
        </Select.Content>
      </Select.Root>
      <Select.Root type="single" bind:value={freshnessFilter}>
        <Select.Trigger class="w-full" aria-label={m.allowances_filter_freshness()}>{freshnessFilterLabel}</Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="all">{m.allowances_filter_all()}</Select.Item>
            <Select.Item value="fresh">{m.allowances_status_fresh()}</Select.Item>
            <Select.Item value="stale">{m.allowances_status_stale()}</Select.Item>
            <Select.Item value="error">{m.allowances_status_error()}</Select.Item>
          </Select.Group>
        </Select.Content>
      </Select.Root>
    </section>

    <section
      class={['relative overflow-hidden rounded-xl border px-4 py-3', conditionTone(overallCondition)]}
      aria-label={m.allowances_condition_title()}>
      <div class="absolute inset-y-0 left-0 w-1 bg-current opacity-60"></div>
      <div class="grid items-center gap-3 sm:grid-cols-[minmax(9rem,1fr)_repeat(2,minmax(0,1fr))]">
        <div>
          <p class="text-xs font-medium uppercase tracking-[0.12em] text-muted-foreground">{m.allowances_condition_title()}</p>
          <p class="mt-0.5 text-base font-semibold">{conditionLabel(overallCondition)}</p>
        </div>
        <p class="font-technical text-sm tabular-nums">
          {lowestRemaining == null
            ? m.allowances_lowest_remaining({ value: '–' })
            : m.allowances_lowest_remaining({ value: formatAllowancePercent(lowestRemaining, localeState.current) })}
        </p>
        <p class="font-technical text-sm tabular-nums">
          {timeline[0]
            ? m.allowances_next_reset({ time: formatLogTime(timeline[0].allowance.reset_at, localeState.current) })
            : m.allowances_no_upcoming_reset()}
        </p>
      </div>
    </section>

    <div class="grid min-w-0 items-start gap-3 xl:grid-cols-[minmax(0,2fr)_minmax(17rem,1fr)]">
      <Card.Root class="min-w-0 gap-0 overflow-hidden" size="sm">
        <Card.Header class="border-b"><h2 class="text-base font-semibold">{m.allowances_matrix_title()}</h2></Card.Header>
        <Card.Content class="p-0">
          {#if visibleProviders.length === 0}
            <p class="p-8 text-center text-sm text-muted-foreground">{m.allowances_filter_empty()}</p>
          {:else}
            <div class="route-desktop-table">
              <DataTable
                data={allowanceMatrixRows}
                columns={allowanceColumns}
                labels={tableLabels}
                getRowId={allowanceRowId}
                ariaLabel={m.allowances_matrix_title()}
                size="small"
                grouping={allowanceGrouping}
                expanded={true}
                columnVisibility={allowanceColumnVisibility}
                groupRow={allowanceGroupRow}
                rowClass={allowanceRowClass}
                class="gap-0 [&_[data-slot=data-table-viewport]]:rounded-none [&_[data-slot=data-table-viewport]]:border-0"
                tableClass="min-w-[46rem] [&_[data-slot=table-header]]:bg-muted/20" />
            </div>
            <div class="route-mobile-list">
              {#each visibleProviders as provider (provider.snapshot.provider_id)}
                <section class="border-b last:border-b-0">
                  <div
                    class="flex min-h-14 items-center justify-between gap-3 bg-muted/35 px-3 py-2.5"
                    data-testid={`allowance-mobile-provider-${provider.snapshot.provider_id}`}>
                    {@render allowanceProviderSummary(provider.snapshot, provider.allowances)}
                  </div>
                  {#each provider.allowances as allowance (allowance.key)}
                    {@render mobileAllowanceRow(allowance)}
                  {/each}
                </section>
              {/each}
            </div>
          {/if}
        </Card.Content>
      </Card.Root>

      <div class="grid min-w-0 gap-3">
        <Card.Root class="gap-0" size="sm">
          <Card.Header class="border-b">
            <div class="flex items-center gap-2"><Clock3Icon class="size-4 text-primary" /><h2 class="text-base font-semibold">{m.allowances_timeline_title()}</h2></div>
          </Card.Header>
          <Card.Content class="pt-3">
            {#if timeline.length === 0}
              <p class="text-sm text-muted-foreground">{m.allowances_timeline_empty()}</p>
            {:else}
              <ol class="relative ml-2 border-l">
                {#each timeline as item (`${item.snapshot.provider_id}:${item.allowance.key}`)}
                  <li class="relative pb-3 pl-5 last:pb-0">
                    <span class="absolute -left-1.5 top-1 size-3 rounded-full border-2 border-background bg-primary"></span>
                    <p class="font-technical text-sm font-medium tabular-nums">{formatLogTime(item.allowance.reset_at, localeState.current)}</p>
                    <p class="mt-1 text-sm text-muted-foreground">{item.snapshot.provider_name} · {allowanceLabel(item.allowance)}</p>
                  </li>
                {/each}
              </ol>
            {/if}
          </Card.Content>
        </Card.Root>

        <Card.Root class="gap-0" size="sm">
          <Card.Header class="border-b">
            <div class="flex items-center gap-2"><TrendingDownIcon class="size-4 text-primary" /><h2 class="text-base font-semibold">{m.allowances_forecast_title()}</h2></div>
            <Card.Description>{m.allowances_forecast_basis()}</Card.Description>
          </Card.Header>
          <Card.Content class="pt-3">
            <div class="grid grid-cols-3 gap-2 text-center">
              <div class="rounded-lg bg-emerald-500/8 px-2 py-3"><p class="font-technical text-lg font-semibold tabular-nums">{forecastSummary.noRisk}</p><p class="text-xs text-muted-foreground">{m.allowances_forecast_no_risk({ count: forecastSummary.noRisk })}</p></div>
              <div class="rounded-lg bg-red-500/8 px-2 py-3"><p class="font-technical text-lg font-semibold tabular-nums">{forecastSummary.willExhaust}</p><p class="text-xs text-muted-foreground">{m.allowances_forecast_will_exhaust({ count: forecastSummary.willExhaust })}</p></div>
              <div class="rounded-lg bg-muted px-2 py-3"><p class="font-technical text-lg font-semibold tabular-nums">{forecastSummary.unknown}</p><p class="text-xs text-muted-foreground">{m.allowances_forecast_unknown({ count: forecastSummary.unknown })}</p></div>
            </div>
            <p class="mt-4 border-t pt-4 text-sm text-muted-foreground">
              {forecastSummary.lowestProjected == null
                ? m.allowances_forecast_no_projection()
                : m.allowances_forecast_lowest({ value: formatAllowancePercent(forecastSummary.lowestProjected, localeState.current) })}
            </p>
            {#if forecastSummary.risks.length > 0}
              <ul class="mt-4 grid gap-2 border-t pt-4">
                {#each forecastSummary.risks as item (`${item.snapshot.provider_id}:${item.allowance.key}`)}
                  <li class="text-sm">
                    {item.allowance.forecast.exhausts_at == null
                      ? `${item.snapshot.provider_name} · ${allowanceLabel(item.allowance)}`
                      : m.allowances_forecast_exhausts_at({ provider: item.snapshot.provider_name, item: allowanceLabel(item.allowance), time: formatLogTime(item.allowance.forecast.exhausts_at, localeState.current) })}
                  </li>
                {/each}
              </ul>
            {/if}
          </Card.Content>
        </Card.Root>
      </div>
    </div>
  {/if}
</div>

<style>
.allowances-page {
  gap: 1rem;
}
</style>
