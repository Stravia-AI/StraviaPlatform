<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import LightbulbIcon from '@lucide/svelte/icons/lightbulb'

import { admin, currencyQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import MetricStrip, { type MetricItem } from '#lib/components/metric-strip.svelte'
import ProviderCostPlanEditor from '#lib/components/provider-cost-plan-editor.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'
import { formatLogTime } from '#lib/format.js'
import { formatCostPlanPrice, formatLowerBoundMoney, formatMoney, formatValueMultiple } from '#lib/money.js'
import {
  costDraftFromSuggestion,
  costSuggestionFromAllowance,
  type CostSuggestion,
  type ProviderCostDraftFields,
} from '#lib/provider-cost-draft.js'
import { liveStatsWindow } from '#lib/stats-window.js'
import type { NewProviderCostPlan, Provider, ProviderCostPlan, ProviderStats } from '#lib/types.js'

interface Props {
  provider: Provider
  /** 该供应商的通道是否提供额度读取；只有提供时才可能识别出计费方式。 */
  supportsAllowance: boolean
}

let { provider, supportsAllowance }: Props = $props()

const HISTORY_LIMIT = 5
const DAY_MS = 86_400_000
const windows = [
  { days: '1', label: () => m.cost_value_window_24h() },
  { days: '7', label: () => m.cost_value_window_7d() },
  { days: '30', label: () => m.cost_value_window_30d() },
] as const

const queryClient = useQueryClient()
const costPlansQuery = createQuery(() => ({
  queryKey: ['provider-cost-plans', provider.id],
  queryFn: () => admin.providers.costPlans(provider.id),
}))
const currencyQuery = createQuery(() => currencyQueryOptions())
const customCurrency = $derived(currencyQuery.data ?? null)

const costPlans = $derived(costPlansQuery.data ?? [])
const plan = $derived(costPlans[0] ?? provider.cost_plan ?? null)
const basis = $derived(plan?.basis ?? 'usage')
const recentPlans = $derived(costPlans.slice(0, HISTORY_LIMIT))

// 额度接口的识别结果只作建议：不自动写入，由用户采用后经 Sheet 显式保存。
const allowanceQuery = createQuery(() => ({
  queryKey: ['provider-cost-suggestion', provider.id],
  queryFn: () => admin.allowances.get(provider.id),
  enabled: supportsAllowance && provider.is_enabled,
  retry: false,
  staleTime: 60_000,
}))
// 等币种查询落定再给建议，避免先显示美元标价、随后又跳成自定义币种。
const suggestion = $derived(
  costPlansQuery.data && !currencyQuery.isPending
    ? costSuggestionFromAllowance(allowanceQuery.data?.billing, allowanceQuery.data?.plan_label, plan, customCurrency)
    : null,
)
let dismissed = $state('')
const visibleSuggestion = $derived(suggestion && JSON.stringify(suggestion) !== dismissed ? suggestion : null)

let windowDays = $state<(typeof windows)[number]['days']>('1')
const statsQuery = createQuery(() => ({
  queryKey: ['stats-providers', 'provider-cost', provider.id, windowDays],
  queryFn: async (): Promise<ProviderStats | null> => {
    const rows = await admin.stats.providers(liveStatsWindow(Number(windowDays) * DAY_MS))
    return rows.find((row) => row.provider_id === provider.id) ?? null
  },
  enabled: basis !== 'none',
  refetchInterval: 30_000,
}))

let editorOpen = $state(false)
let prefill = $state<ProviderCostDraftFields | null>(null)

function openEditor(draft: ProviderCostDraftFields | null = null): void {
  prefill = draft
  editorOpen = true
}

function adopt(item: CostSuggestion): void {
  openEditor(costDraftFromSuggestion(item, customCurrency !== null))
}

async function appendPlan(input: NewProviderCostPlan): Promise<void> {
  const saved = await admin.providers.appendCostPlan(provider.id, input)
  queryClient.setQueryData<ProviderCostPlan[]>(['provider-cost-plans', provider.id], (current) => [
    saved,
    ...(current ?? []),
  ])
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ['provider-cost-plans', provider.id] }),
    queryClient.invalidateQueries({ queryKey: ['providers'] }),
    queryClient.invalidateQueries({ queryKey: ['stats-providers'] }),
  ])
}

function basisLabel(item: ProviderCostPlan | null): string {
  const kind = item?.basis ?? 'usage'
  if (kind === 'none') return m.cost_basis_none()
  if (kind === 'usage') return m.cost_basis_usage()
  return `${m.cost_basis_subscription()} · ${item?.period === 'year' ? m.cost_period_year() : m.cost_period_month()}`
}

function priceLabel(item: ProviderCostPlan): string {
  const unit = item.period === 'year' ? m.cost_plan_per_year() : m.cost_plan_per_month()
  return `${formatCostPlanPrice(item.price, customCurrency)} ${unit}`
}

function historyLabel(item: ProviderCostPlan): string {
  return item.basis === 'subscription'
    ? `${basisLabel(item)} · ${formatCostPlanPrice(item.price, customCurrency)}`
    : basisLabel(item)
}

function valueMetrics(row: ProviderStats): MetricItem[] | null {
  if (basis === 'usage') {
    if (!row.cost) return null
    return [
      { label: m.cost_usage_cost(), value: formatLowerBoundMoney(row.cost, row.currency) },
      { label: m.cost_value_priced_calls(), value: String(row.cost.known_count) },
    ]
  }
  const subscription = row.subscription
  if (!subscription) return null
  return [
    {
      label: m.cost_usage_equivalent(),
      value: formatLowerBoundMoney(subscription.usage_equivalent_cost, row.currency),
    },
    { label: m.cost_subscription_cost(), value: formatMoney(subscription.cost, row.currency) },
    { label: m.cost_value_multiple(), value: formatValueMultiple(subscription) },
  ]
}

function suggestionTitle(item: CostSuggestion): string {
  if (item.kind === 'usage') return m.cost_suggestion_usage()
  return item.planLabel
    ? m.cost_suggestion_subscription({ plan: item.planLabel })
    : m.cost_suggestion_subscription_plain()
}

function suggestionPrice(item: CostSuggestion): string {
  if (item.kind !== 'subscription' || item.price === null) return ''
  const unit = item.period === 'year' ? m.cost_plan_per_year() : m.cost_plan_per_month()
  return `${formatCostPlanPrice(item.price, customCurrency)} ${unit}`
}
</script>

<div class="mx-auto flex w-full max-w-(--content-column-max-width) flex-col gap-8">
  <section class="route-section" aria-labelledby="provider-cost-plan-title">
    <h2 id="provider-cost-plan-title" class="route-section-title">{m.cost_plan_section()}</h2>
    {#if costPlansQuery.error && costPlansQuery.data === undefined}
      <RequestFailure
        title={m.cost_plan_not_loaded()}
        message={localizeBackendErrorMessage(costPlansQuery.error)}
        retry={() => void costPlansQuery.refetch()}
        retrying={costPlansQuery.isFetching} />
    {:else if costPlansQuery.isPending && !provider.cost_plan}
      <div class="flex flex-col gap-3 py-1" aria-busy="true">
        <Skeleton class="h-5 w-40" />
        <Skeleton class="h-5 w-56" />
      </div>
    {:else}
      <div>
        <SettingRow title={m.cost_plan_basis()} onedit={() => openEditor()}>
          <Badge variant="secondary">{basisLabel(plan)}</Badge>
        </SettingRow>
        {#if plan?.basis === 'subscription'}
          <SettingRow title={m.cost_plan_price()} value={priceLabel(plan)} />
        {/if}
        {#if costPlans[0]}
          <SettingRow title={m.cost_plan_effective_since()} value={formatLogTime(costPlans[0].effective_from)} />
        {/if}
      </div>
      {#if visibleSuggestion}
        <div
          class="mt-3 flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border bg-card px-3 py-2 text-sm"
          role="region"
          aria-label={m.cost_suggestion_label()}>
          <LightbulbIcon class="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          <div class="flex min-w-0 flex-[1_1_14rem] flex-wrap items-baseline gap-x-3">
            <span class="font-medium">{suggestionTitle(visibleSuggestion)}</span>
            {#if suggestionPrice(visibleSuggestion)}
              <span class="font-technical tabular-nums">{suggestionPrice(visibleSuggestion)}</span>
            {/if}
          </div>
          <div class="ms-auto flex gap-2">
            <Button variant="ghost" size="sm" onclick={() => (dismissed = JSON.stringify(visibleSuggestion))}
              >{m.cost_suggestion_dismiss()}</Button>
            <Button size="sm" onclick={() => adopt(visibleSuggestion)}>
              {visibleSuggestion.kind === 'subscription' && visibleSuggestion.price === null
                ? m.cost_suggestion_set_subscription()
                : m.cost_suggestion_adopt()}
            </Button>
          </div>
        </div>
      {/if}
    {/if}
  </section>

  <section class="route-section" aria-labelledby="provider-cost-value-title">
    <div class="route-section-header">
      <h2 id="provider-cost-value-title" class="route-section-title">{m.cost_value_section()}</h2>
      {#if basis !== 'none'}
        <Tabs.Root bind:value={windowDays}>
          <Tabs.List aria-label={m.cost_value_window()}>
            {#each windows as item (item.days)}
              <Tabs.Trigger value={item.days}>{item.label()}</Tabs.Trigger>
            {/each}
          </Tabs.List>
        </Tabs.Root>
      {/if}
    </div>
    {#if basis === 'none'}
      <div class="flex flex-wrap items-center justify-between gap-3">
        <p class="text-sm text-muted-foreground">{m.cost_value_not_tracked()}</p>
        <Button variant="outline" size="sm" onclick={() => openEditor()}>{m.cost_plan_edit_title()}</Button>
      </div>
    {:else if statsQuery.error && statsQuery.data === undefined}
      <RequestFailure
        title={m.cost_value_not_loaded()}
        message={localizeBackendErrorMessage(statsQuery.error)}
        retry={() => void statsQuery.refetch()}
        retrying={statsQuery.isFetching} />
    {:else if statsQuery.isPending}
      <MetricStrip loading loadingLabel={m.common_loading()} placeholderCount={basis === 'usage' ? 2 : 3} />
    {:else}
      {@const metrics = statsQuery.data ? valueMetrics(statsQuery.data) : null}
      {#if metrics}
        <MetricStrip {metrics} label={m.cost_value_section()} />
      {:else}
        <p class="text-sm text-muted-foreground">{m.cost_value_no_usage()}</p>
      {/if}
    {/if}
  </section>

  <section class="route-section" aria-labelledby="provider-cost-history-title">
    <h2 id="provider-cost-history-title" class="route-section-title">{m.cost_plan_history()}</h2>
    {#if costPlansQuery.error && costPlansQuery.data === undefined}
      <p class="text-sm text-muted-foreground">{m.cost_plan_not_loaded()}</p>
    {:else if costPlansQuery.isPending}
      <div class="flex flex-col gap-2 py-1" aria-busy="true">
        <Skeleton class="h-4 w-40" />
        <Skeleton class="h-4 w-56" />
      </div>
    {:else if recentPlans.length === 0}
      <p class="text-sm text-muted-foreground">{m.cost_plan_no_history()}</p>
    {:else}
      <ol class="flex flex-col gap-4" aria-label={m.cost_plan_history_aria()}>
        {#each recentPlans as item, index (item.id)}
          <li class="relative grid grid-cols-[1rem_minmax(0,1fr)_auto] items-center gap-x-3">
            {#if index < recentPlans.length - 1}
              <span class="absolute top-1/2 bottom-[calc(-1rem-50%)] left-[7.5px] w-px bg-border" aria-hidden="true"
              ></span>
            {/if}
            <span
              class={[
                'relative z-10 ms-[3px] size-[9px] rounded-full border-2 bg-background',
                index === 0 ? 'border-primary bg-primary' : 'border-input',
              ]}
              aria-hidden="true"></span>
            <span class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm font-medium">
              {historyLabel(item)}
              {#if index === 0}<Badge variant="secondary">{m.cost_plan_current()}</Badge>{/if}
            </span>
            <time class="font-technical text-xs text-muted-foreground tabular-nums"
              >{formatLogTime(item.effective_from)}</time>
          </li>
        {/each}
      </ol>
      {#if costPlans.length > HISTORY_LIMIT}
        <p class="mt-3 text-xs text-muted-foreground">{m.cost_plan_history_recent({ count: HISTORY_LIMIT })}</p>
      {/if}
    {/if}
  </section>
</div>

<ProviderCostPlanEditor bind:open={editorOpen} {plan} {prefill} {customCurrency} onsave={appendPlan} />
