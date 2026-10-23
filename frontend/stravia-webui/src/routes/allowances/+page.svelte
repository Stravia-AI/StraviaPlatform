<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { onMount } from 'svelte'
import { useQueryClient } from '@tanstack/svelte-query'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import CircleHelpIcon from '@lucide/svelte/icons/circle-help'
import ShieldCheckIcon from '@lucide/svelte/icons/shield-check'
import GaugeIcon from '@lucide/svelte/icons/gauge'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Progress } from '#lib/components/ui/progress/index.js'
import * as Collapsible from '#lib/components/ui/collapsible/index.js'
import * as Accordion from '#lib/components/ui/accordion/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SearchIcon from '@lucide/svelte/icons/search'
import { SvelteMap, SvelteSet } from 'svelte/reactivity'
import { toast } from 'svelte-sonner'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatLogTime } from '#lib/format.js'
import { localeState } from '#lib/localization.svelte.js'
import { formatAllowanceAmount, formatAllowancePercent } from '#lib/provider-allowance-format.js'
import { ProviderAllowanceRead } from '#lib/provider-allowance-read.svelte.js'
import type { ProviderAllowanceReadEntry } from '#lib/provider-allowance-read.js'
import {
  effectiveAllowanceCondition,
  forecastBucket,
  nextRelevantResetAt,
  remainingPercent,
  summarizeForecast,
  usableRemainingPercent,
  worstAllowanceCondition,
} from '#lib/provider-allowance-summary.js'
import type {
  Allowance,
  AllowanceCondition,
  ModelAllowance,
  ProviderAllowanceErrorCategory,
  ProviderAllowanceSnapshot,
  ProviderAllowanceStatus,
  ProviderAllowanceTarget,
} from '#lib/types.js'
import PageHeader from '#lib/components/page-header.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { Badge, type BadgeVariant } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import * as Table from '#lib/components/ui/table/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'
import AllowanceSuspension from '#lib/components/allowance-suspension.svelte'
import { cn } from '#lib/utils.js'

interface VisibleProvider extends ProviderAllowanceReadEntry {
  allowances: Allowance[]
}

interface VisibleAllowance {
  snapshot: ProviderAllowanceSnapshot
  allowance: Allowance
}

type AllowanceValueMode = 'remaining' | 'used'

// 仅是本机展示偏好，不进入管理面配置
const VALUE_MODE_STORAGE_KEY = 'stravia:allowances:value-mode'
const TIMELINE_PREVIEW_LIMIT = 10
const TIMELINE_PREVIEW_WINDOW_MS = 7 * 24 * 60 * 60 * 1000

const reading = new ProviderAllowanceRead(useQueryClient(), { providers: admin.providers.list, ...admin.allowances })
const readState = $derived(reading.snapshot)
let searchQuery = $state('')
let catalogFilter = $state('all')
let conditionFilter = $state<'all' | AllowanceCondition>('all')
let freshnessFilter = $state<'all' | ProviderAllowanceStatus>('all')
let valueMode = $state<AllowanceValueMode>('remaining')
const expandedProviderIds = new SvelteSet<string>()
let timelineExpanded = $state(false)
let exhaustedOpen = $state(false)
let willExhaustOpen = $state(false)
let hoveredAllowanceId = $state<string | null>(null)
let focusedAllowanceId = $state<string | null>(null)

onMount(() => {
  if (localStorage.getItem(VALUE_MODE_STORAGE_KEY) === 'used') valueMode = 'used'
})

function changeValueMode(next: string): void {
  // Tabs 回调给出 string；收窄为 ValueMode
  if (next !== 'remaining' && next !== 'used') return
  valueMode = next
  localStorage.setItem(VALUE_MODE_STORAGE_KEY, next)
}

function setProviderExpanded(providerId: string, open: boolean): void {
  if (open) expandedProviderIds.add(providerId)
  else expandedProviderIds.delete(providerId)
}

const collator = $derived(new Intl.Collator(localeState.current, { sensitivity: 'base', numeric: true }))
const hasEntries = $derived(readState.entries.length > 0)
const entries = $derived.by(() =>
  [...readState.entries].sort(
    (left, right) =>
      collator.compare(left.target.provider_name, right.target.provider_name) ||
      left.target.provider_id.localeCompare(right.target.provider_id),
  ),
)
const catalogOptions = $derived.by(() => {
  const options = new SvelteMap<string, string>()
  for (const entry of entries) {
    const value = catalogValue(entry.target)
    options.set(value, `${entry.target.catalog_provider_id} / ${entry.target.channel}`)
  }
  return [...options]
    .map(([value, label]) => ({ value, label }))
    .sort((left, right) => collator.compare(left.label, right.label))
})
const catalogFilterLabel = $derived(
  catalogFilter === 'all'
    ? m.allowances_filter_catalog_all()
    : m.allowances_filter_catalog_value({
        value: catalogOptions.find((option) => option.value === catalogFilter)?.label ?? catalogFilter,
      }),
)
const conditionFilterLabel = $derived(
  conditionFilter === 'all'
    ? m.allowances_filter_condition_all()
    : m.allowances_filter_condition_value({ value: conditionLabel(conditionFilter) }),
)
const freshnessFilterLabel = $derived(
  freshnessFilter === 'all'
    ? m.allowances_filter_freshness_all()
    : m.allowances_filter_freshness_value({ value: statusPresentation(freshnessFilter).label }),
)
const visibleProviders = $derived.by((): VisibleProvider[] => {
  const query = searchQuery.trim().toLocaleLowerCase(localeState.current)
  return entries.flatMap((entry) => {
    if (query) {
      const { provider_name, catalog_provider_id, channel } = entry.target
      const haystack = [provider_name, catalog_provider_id, channel].join('\n').toLocaleLowerCase(localeState.current)
      if (!haystack.includes(query)) return []
    }
    if (catalogFilter !== 'all' && catalogValue(entry.target) !== catalogFilter) return []
    // Pending providers have no status yet; keep them visible under any filter
    // since their freshness/condition cannot be evaluated yet.
    if (freshnessFilter !== 'all' && entry.snapshot != null && entry.snapshot.status !== freshnessFilter) return []
    const allowances =
      entry.snapshot == null
        ? []
        : conditionFilter === 'all'
          ? entry.snapshot.allowances
          : entry.snapshot.allowances.filter((allowance) => effectiveAllowanceCondition(allowance) === conditionFilter)
    if (conditionFilter !== 'all' && entry.snapshot != null && allowances.length === 0) return []
    return [{ ...entry, allowances }]
  })
})
const allResolved = $derived(
  visibleProviders.every((entry) => entry.snapshot != null || entry.failed || entry.credentialInvalid),
)
const visibleAllowances = $derived(
  visibleProviders.flatMap(({ snapshot, allowances }) =>
    snapshot == null ? [] : allowances.map((allowance) => ({ snapshot, allowance })),
  ),
)
const valueModeLabel = $derived(valueMode === 'used' ? m.allowances_used() : m.allowances_remaining())
const overallCondition = $derived(
  worstAllowanceCondition(visibleAllowances.map(({ allowance }) => effectiveAllowanceCondition(allowance))),
)
const lowestRemaining = $derived.by(() => {
  const values = visibleAllowances
    .map(({ allowance }) => usableRemainingPercent(allowance))
    .filter((value): value is number => value != null && Number.isFinite(value))
  return values.length ? Math.min(...values) : undefined
})
const timeline = $derived.by(() =>
  visibleAllowances
    .filter(
      (item): item is VisibleAllowance & { allowance: Allowance & { reset_at: number } } =>
        item.allowance.reset_at != null,
    )
    .sort(
      (left, right) =>
        left.allowance.reset_at - right.allowance.reset_at ||
        collator.compare(left.snapshot.provider_name, right.snapshot.provider_name) ||
        collator.compare(allowanceLabel(left.allowance), allowanceLabel(right.allowance)),
    ),
)
const timelineNow = $derived.by(() => {
  // 时间窗口跟随读取快照更新，不因搜索、展开或悬停重算，也不增加独立计时器。
  void readState
  return Date.now()
})
// 展示范围与预报统计分开；过去的重置时间仍可能携带有效的耗尽证据。
const futureTimeline = $derived(timeline.filter(({ allowance }) => allowance.reset_at > timelineNow))
const timelinePreview = $derived(
  futureTimeline
    .filter(({ allowance }) => allowance.reset_at <= timelineNow + TIMELINE_PREVIEW_WINDOW_MS)
    .slice(0, TIMELINE_PREVIEW_LIMIT),
)
const displayedTimeline = $derived(timelineExpanded ? futureTimeline : timelinePreview)
const highlightedAllowanceId = $derived.by(() => {
  const id = hoveredAllowanceId ?? focusedAllowanceId
  return displayedTimeline.some(
    ({ snapshot, allowance }) => allowanceElementId(snapshot.provider_id, allowance.key) === id,
  )
    ? id
    : null
})
const nextResetAt = $derived(nextRelevantResetAt(visibleAllowances.map(({ allowance }) => allowance)))
const emptyWindows = $derived.by(() =>
  visibleAllowances
    .filter(({ allowance }) => effectiveAllowanceCondition(allowance) === 'exhausted')
    .sort(
      (left, right) =>
        (left.allowance.reset_at ?? Number.POSITIVE_INFINITY) -
          (right.allowance.reset_at ?? Number.POSITIVE_INFINITY) ||
        collator.compare(left.snapshot.provider_name, right.snapshot.provider_name) ||
        collator.compare(allowanceLabel(left.allowance), allowanceLabel(right.allowance)),
    ),
)
const forecastSummary = $derived.by(() => {
  const counts = summarizeForecast(timeline.map(({ allowance }) => allowance))
  const exhaustedItems = timeline.filter(({ allowance }) => forecastBucket(allowance) === 'exhausted')
  const willExhaustItems = timeline.filter(({ allowance }) => forecastBucket(allowance) === 'will_exhaust')
  return { ...counts, exhaustedItems, willExhaustItems }
})
const latestFetchedAt = $derived.by(() => {
  const timestamps = entries
    .map((entry) => entry.snapshot?.fetched_at)
    .filter((value): value is string => Boolean(value))
    .sort()
  return timestamps.at(-1)
})

function allowanceElementId(providerId: string, key: string): string {
  return `allowance-${encodeURIComponent(providerId)}:${encodeURIComponent(key)}`
}

function locateAllowance(id: string): void {
  const target = document.getElementById(id)
  if (!target) return
  hoveredAllowanceId = null
  target.focus({ preventScroll: true })
  target.scrollIntoView({ block: 'center', behavior: 'instant' })
}

function catalogValue(target: Pick<ProviderAllowanceTarget, 'catalog_provider_id' | 'channel'>): string {
  return `${target.catalog_provider_id}::${target.channel}`
}

async function refreshAll(): Promise<void> {
  const outcome = await reading.refreshAll()
  if (outcome.status === 'failed') toast.error(localizeBackendErrorMessage(outcome.error))
  else if (outcome.status === 'refreshed') toast.success(m.allowances_refreshed_all())
}

async function refreshProvider(provider: VisibleProvider): Promise<void> {
  const outcome = await reading.refreshProvider(provider.target.provider_id)
  if (outcome.status === 'failed') toast.error(localizeBackendErrorMessage(outcome.error))
  else if (outcome.status === 'refreshed') {
    toast.success(m.allowances_refreshed_provider({ provider: provider.target.provider_name }))
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

function forecastItemCopy(item: VisibleAllowance): string {
  const provider = item.snapshot.provider_name
  const label = allowanceLabel(item.allowance)
  const reset = item.allowance.reset_at
  if (forecastBucket(item.allowance) === 'exhausted') {
    return item.allowance.forecast.exhausts_at == null
      ? m.allowances_forecast_exhausted_item_resets({
          provider,
          item: label,
          reset: formatLogTime(reset, localeState.current),
        })
      : m.allowances_forecast_exhausted_at_resets({
          provider,
          item: label,
          time: formatLogTime(item.allowance.forecast.exhausts_at, localeState.current),
          reset: formatLogTime(reset, localeState.current),
        })
  }
  return item.allowance.forecast.exhausts_at == null
    ? `${provider} · ${label}`
    : m.allowances_forecast_exhausts_at({
        provider,
        item: label,
        time: formatLogTime(item.allowance.forecast.exhausts_at, localeState.current),
      })
}

function usedPercent(allowance: Allowance): number | undefined {
  if (allowance.used_percent != null && Number.isFinite(allowance.used_percent)) return allowance.used_percent
  const remaining = remainingPercent(allowance)
  return remaining == null ? undefined : 100 - remaining
}

// 账户余额本身就是剩余金额，没有可对照的上限，不随剩余/已用切换
function modePercent(allowance: Allowance): number | undefined {
  if (allowance.kind === 'balance') return undefined
  return valueMode === 'used' ? usedPercent(allowance) : remainingPercent(allowance)
}

function valueDisplay(allowance: Allowance): string {
  const percent = modePercent(allowance)
  if (percent != null) return formatAllowancePercent(percent, localeState.current)
  const amount = allowance.kind === 'balance' || valueMode === 'remaining' ? allowance.remaining : allowance.used
  return formatAllowanceAmount(amount, localeState.current)
}

function meterToneClass(condition: AllowanceCondition | undefined): string {
  switch (condition) {
    case 'exhausted':
      return '[&_[data-slot=progress-indicator]]:bg-destructive'
    case 'tight':
      return '[&_[data-slot=progress-indicator]]:bg-warning'
    default:
      return ''
  }
}

function conditionTextClass(condition: AllowanceCondition | undefined): string {
  return condition === 'exhausted' ? 'text-destructive' : condition === 'tight' ? 'text-warning' : ''
}

function conditionStatusTone(condition: AllowanceCondition | undefined): 'healthy' | 'warning' | 'error' | 'neutral' {
  switch (condition) {
    case 'normal':
      return 'healthy'
    case 'tight':
      return 'warning'
    case 'exhausted':
      return 'error'
    default:
      return 'neutral'
  }
}

function resetDisplay(allowance: Allowance): string {
  return allowance.reset_at != null ? formatLogTime(allowance.reset_at, localeState.current) : '–'
}

function triggeredSuspension(provider: VisibleProvider, allowance: Allowance): boolean {
  return Boolean(provider.snapshot?.suspension?.triggered_keys.includes(allowance.key))
}

function allowanceLabel(allowance: Allowance): string {
  // 各供应商余额 key 不一致（credits、credits_balance_cny、balance_usd 等），统一显示为账户余额
  if (
    allowance.key === 'credits' ||
    allowance.key.startsWith('credits_balance') ||
    allowance.key.startsWith('balance_')
  ) {
    return m.allowances_label_balance()
  }
  switch (allowance.key) {
    case '5h':
    case 'five_hour':
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
  }
  // 只识别已知上游标签组合；未知模型组或窗口保持原文，不从持久化 key 推断。
  const disabled = allowance.label.endsWith(' (disabled)')
  const label = disabled ? allowance.label.slice(0, -' (disabled)'.length) : allowance.label
  let localized: string
  switch (label) {
    case 'Claude and GPT models / Five Hour Limit Remaining (5h)':
      localized = m.allowances_label_models_five_hour_remaining({ models: m.allowances_label_claude_gpt_models() })
      break
    case 'Claude and GPT models / Weekly Limit Remaining (weekly)':
      localized = m.allowances_label_models_weekly_remaining({ models: m.allowances_label_claude_gpt_models() })
      break
    case 'Gemini Models / Five Hour Limit Remaining (5h)':
      localized = m.allowances_label_models_five_hour_remaining({ models: m.allowances_label_gemini_models() })
      break
    case 'Gemini Models / Weekly Limit Remaining (weekly)':
      localized = m.allowances_label_models_weekly_remaining({ models: m.allowances_label_gemini_models() })
      break
    default:
      return allowance.label
  }
  return disabled ? m.allowances_label_disabled({ label: localized }) : localized
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

{#snippet conditionMark(condition: AllowanceCondition | undefined)}
  {#if condition === 'tight'}
    <span class="h-0.5 w-2.5 shrink-0 rounded-full bg-warning" aria-hidden="true"></span>
    <span class="sr-only">{conditionLabel(condition)}</span>
  {:else if condition === 'exhausted'}
    <span class="size-1.5 shrink-0 rotate-45 rounded-[1px] bg-destructive" aria-hidden="true"></span>
    <span class="sr-only">{conditionLabel(condition)}</span>
  {/if}
{/snippet}

{#snippet allowanceValue(allowance: Allowance, className: string)}
  {@const condition = effectiveAllowanceCondition(allowance)}
  <span
    class={cn(
      'font-technical inline-flex items-center gap-1.5 whitespace-nowrap tabular-nums',
      conditionTextClass(condition),
      className,
    )}>
    {@render conditionMark(condition)}{valueDisplay(allowance)}
  </span>
{/snippet}

{#snippet allowanceMeter(allowance: Allowance, className: string, decorative: boolean)}
  {@const percent = modePercent(allowance)}
  {#if percent != null}
    <Progress
      value={Math.min(100, Math.max(0, percent))}
      class={cn('h-1.5 shrink-0', meterToneClass(effectiveAllowanceCondition(allowance)), className)}
      aria-hidden={decorative ? 'true' : undefined}
      aria-label={decorative ? undefined : `${allowanceLabel(allowance)} ${valueModeLabel}`} />
  {/if}
{/snippet}

{#snippet allowanceChips(provider: VisibleProvider)}
  {@const providerId = provider.target.provider_id}
  {#if provider.pending}
    <div class="flex min-h-10 items-center" data-testid={`allowance-loading-${providerId}`}>
      <Spinner aria-label={m.allowances_loading()} />
    </div>
  {:else if provider.failed}
    <div class="relative z-10 flex flex-wrap items-center gap-3" data-testid={`allowance-failed-${providerId}`}>
      <span class="inline-flex items-center gap-1.5 text-sm text-destructive" role="status">
        <span class="size-1.5 shrink-0 rotate-45 rounded-[1px] bg-destructive" aria-hidden="true"></span>
        {m.allowances_load_failed()}
      </span>
      {#if provider.canRetry}
        <Button variant="outline" size="sm" onclick={() => reading.retryProvider(providerId)}
          >{m.common_retry()}</Button>
      {/if}
    </div>
  {:else if provider.allowances.length > 0}
    <!-- 轨道宽度只取决于容器宽度且有上限：新增宽度容纳更多对齐的额度项，而不是拉长单个进度条 -->
    <ul class="grid grid-cols-[repeat(auto-fill,minmax(7rem,9rem))] gap-x-4 gap-y-2">
      {#each provider.allowances as allowance (allowance.key)}
        {@const id = allowanceElementId(providerId, allowance.key)}
        <li
          {id}
          tabindex="-1"
          onfocus={() => (focusedAllowanceId = id)}
          onblur={() => (focusedAllowanceId = null)}
          data-highlighted={highlightedAllowanceId === id ? 'true' : undefined}
          class={cn(
            'grid min-w-0 scroll-m-4 gap-0.5 rounded-sm',
            highlightedAllowanceId === id && 'bg-accent ring-2 ring-primary ring-offset-2 ring-offset-background',
          )}>
          <span class="flex min-w-0 items-center gap-1 text-xs text-muted-foreground">
            <span class="truncate">{allowanceLabel(allowance)}</span>
            {#if provider.snapshot?.guard_supported && allowance.guarded}
              <ShieldCheckIcon class="size-3 shrink-0 text-primary" aria-hidden="true" />
              <span class="sr-only">{m.allowances_guard_enabled()}</span>
            {/if}
          </span>
          <span class="flex items-center gap-2">
            {@render allowanceValue(allowance, 'text-sm')}
            {@render allowanceMeter(allowance, 'w-auto min-w-6 max-w-14 flex-1', true)}
          </span>
        </li>
      {/each}
    </ul>
  {/if}
{/snippet}

{#snippet providerAlerts(provider: VisibleProvider)}
  {@const snapshot = provider.snapshot}
  {#if provider.credentialInvalid}
    <Alert.Root
      variant={snapshot?.status === 'stale' ? 'warning' : 'destructive'}
      role="status"
      data-testid={`allowance-credential-invalid-${provider.target.provider_id}`}>
      <Alert.Description>
        {#if snapshot?.status === 'stale'}{m.allowances_stale_message()}
        {/if}
        {m.allowances_credential_invalid()}
        <Button
          variant="link"
          size="sm"
          href={`/providers/${encodeURIComponent(provider.target.provider_id)}?view=connection`}>
          {m.allowances_manage_providers()}
        </Button>
      </Alert.Description>
    </Alert.Root>
  {:else if snapshot?.error}
    <Alert.Root variant={snapshot.status === 'stale' ? 'warning' : 'destructive'} role="status"
      ><Alert.Description
        >{snapshot.status === 'stale' ? `${m.allowances_stale_message()} ` : ''}{allowanceErrorMessage(
          snapshot.error.category,
        )}</Alert.Description
      ></Alert.Root>
  {/if}
  {#if snapshot?.missing_guarded_keys?.length}
    <Alert.Root variant="warning">
      <Alert.Title>{m.allowances_missing_guards()}</Alert.Title>
      <Alert.Description>
        {#each snapshot.missing_guarded_keys as key (key)}
          <div class="flex flex-wrap items-center gap-2">
            <span class="font-technical break-all">{key}</span>
            <Button
              variant="outline"
              size="sm"
              disabled={!provider.canSaveGuards}
              aria-busy={provider.savingGuards}
              onclick={() => reading.setGuard(snapshot.provider_id, key, false)}
              >{#if provider.savingGuards}<Spinner
                  aria-hidden="true" />{m.allowances_guard_saving()}{:else}{m.allowances_remove_guard({
                  key,
                })}{/if}</Button>
          </div>
        {/each}
      </Alert.Description>
    </Alert.Root>
  {/if}
  {#if provider.refreshError != null}
    <Alert.Root variant="warning" role="status"
      ><Alert.Description>
        {m.allowances_guard_refresh_failed()}
        {localizeBackendErrorMessage(provider.refreshError)}
      </Alert.Description></Alert.Root>
  {/if}
  {#if provider.guardError != null}
    <Alert.Root variant="destructive"
      ><Alert.Description>{localizeBackendErrorMessage(provider.guardError)}</Alert.Description></Alert.Root>
  {/if}
{/snippet}

{#snippet guardControl(provider: VisibleProvider, snapshot: ProviderAllowanceSnapshot, allowance: Allowance)}
  <Switch
    bind:checked={
      () => allowance.guarded,
      (checked: boolean) => {
        void reading.setGuard(snapshot.provider_id, allowance.key, checked)
      }
    }
    disabled={!provider.canSaveGuards}
    aria-busy={provider.savingGuards}
    aria-label={m.allowances_guard_item({ item: allowanceLabel(allowance) })} />
{/snippet}

{#snippet allowanceDetailTable(provider: VisibleProvider)}
  {@const snapshot = provider.snapshot}
  {@const guardSnapshot = snapshot?.guard_supported ? snapshot : undefined}
  <Table.Root
    class="table-fixed"
    aria-label={m.allowances_provider_details({ provider: provider.target.provider_name })}>
    <Table.Header>
      <Table.Row class="hover:bg-transparent">
        <Table.Head class="h-9 ps-0 text-xs font-medium text-muted-foreground">{m.allowances_item()}</Table.Head>
        <Table.Head class="h-9 w-24 text-xs font-medium text-muted-foreground @md:w-44">{valueModeLabel}</Table.Head>
        <Table.Head class="hidden h-9 w-44 text-xs font-medium text-muted-foreground @2xl:table-cell"
          >{m.allowances_reset()}</Table.Head>
        {#if guardSnapshot}
          <Table.Head class="h-9 w-24 whitespace-normal text-xs font-medium text-muted-foreground @md:w-36">
            <span class="inline-flex items-center gap-1">
              {m.allowances_guard_column()}
              <Tooltip.Root delayDuration={0}>
                <Tooltip.Trigger
                  type="button"
                  class="inline-flex shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:text-foreground"
                  aria-label={m.allowances_guard_help_label()}>
                  <CircleHelpIcon class="size-3.5" />
                </Tooltip.Trigger>
                <Tooltip.Content class="max-w-80 text-pretty">{m.allowances_guard_help()}</Tooltip.Content>
              </Tooltip.Root>
            </span>
          </Table.Head>
        {/if}
      </Table.Row>
    </Table.Header>
    <Table.Body>
      {#each provider.allowances as allowance (allowance.key)}
        {@const triggered = triggeredSuspension(provider, allowance)}
        <Table.Row class={cn('last:border-b-0 hover:bg-muted/30', triggered && 'bg-warning/10 hover:bg-warning/15')}>
          <Table.Cell class="ps-0 py-1 whitespace-normal">
            <div class="flex min-w-0 flex-wrap items-center gap-2">
              <span class="min-w-0 break-words font-medium">{allowanceLabel(allowance)}</span>
              {#if triggered}<Badge variant="outline">{m.allowances_suspension_trigger()}</Badge>{/if}
            </div>
            {#if allowance.reset_at != null}
              <p class="font-technical mt-0.5 text-xs text-muted-foreground tabular-nums @2xl:hidden">
                {m.allowances_reset_at({ time: resetDisplay(allowance) })}
              </p>
            {/if}
          </Table.Cell>
          <Table.Cell class="py-1">
            <div class="flex min-w-0 items-center gap-3">
              {@render allowanceValue(allowance, 'min-w-[4.5rem]')}
              {@render allowanceMeter(allowance, 'hidden w-16 @md:block', false)}
            </div>
          </Table.Cell>
          <Table.Cell class="font-technical hidden py-1 text-muted-foreground tabular-nums @2xl:table-cell">
            {resetDisplay(allowance)}
          </Table.Cell>
          {#if guardSnapshot}
            <Table.Cell class="py-1">{@render guardControl(provider, guardSnapshot, allowance)}</Table.Cell>
          {/if}
        </Table.Row>
      {/each}
    </Table.Body>
  </Table.Root>
{/snippet}

{#snippet providerRow(provider: VisibleProvider)}
  {@const providerId = provider.target.provider_id}
  {@const snapshot = provider.snapshot}
  {@const suspension = snapshot ? snapshot.suspension : provider.target.suspension}
  {@const presentation = snapshot && snapshot.status !== 'fresh' ? statusPresentation(snapshot.status) : undefined}
  {@const providerCondition = worstAllowanceCondition(provider.allowances.map(effectiveAllowanceCondition))}
  {@const refreshingProvider = provider.refreshing}
  {@const expanded = expandedProviderIds.has(providerId)}
  {@const hasAlerts =
    provider.credentialInvalid ||
    Boolean(snapshot?.error) ||
    Boolean(snapshot?.missing_guarded_keys?.length) ||
    provider.guardError != null ||
    provider.refreshError != null}
  <li class="@container border-b" data-testid={`allowance-provider-${providerId}`}>
    <Collapsible.Root open={expanded} onOpenChange={(open: boolean) => setProviderExpanded(providerId, open)}>
      <div
        class={cn(
          'relative grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2 px-2 py-2.5 transition-colors duration-[140ms] hover:bg-muted/30 @2xl:grid-cols-[minmax(12rem,16rem)_minmax(0,1fr)_auto]',
          expanded && 'bg-muted/20',
          highlightedAllowanceId?.startsWith(allowanceElementId(providerId, '')) && 'bg-accent/40',
        )}>
        <div class="flex min-w-0 items-start gap-2">
          <ChevronRightIcon
            class={cn(
              'mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)]',
              expanded && 'rotate-90',
            )}
            aria-hidden="true" />
          <div class="min-w-0">
            <div class="flex flex-wrap items-center gap-x-2 gap-y-1">
              <h3 class="min-w-0 font-semibold break-words">
                <!-- 伸展命中区让整行可点击展开；行内其他控件以 z-10 浮于其上 -->
                <Collapsible.Trigger
                  class="cursor-pointer rounded-sm text-start after:absolute after:inset-0 after:content-['']"
                  aria-label={m.allowances_provider_details({ provider: provider.target.provider_name })}>
                  {provider.target.provider_name}
                </Collapsible.Trigger>
              </h3>
              {#if providerCondition}
                <StatusIndicator
                  compact
                  class="text-xs"
                  label={conditionLabel(providerCondition)}
                  tone={conditionStatusTone(providerCondition)} />
              {/if}
              {#if suspension}
                <span class="relative z-10 inline-flex"><AllowanceSuspension {suspension} /></span>
              {/if}
              {#if presentation}<Badge variant={presentation.variant}>{presentation.label}</Badge>{/if}
              {#if snapshot?.plan_label}<span class="text-xs text-muted-foreground">{snapshot.plan_label}</span>{/if}
            </div>
          </div>
        </div>
        <div class="col-span-2 ps-6 @2xl:col-span-1 @2xl:col-start-2 @2xl:row-start-1 @2xl:ps-0">
          {@render allowanceChips(provider)}
        </div>
        <Button
          size="icon"
          class="relative z-10 col-start-2 row-start-1 @2xl:col-start-3"
          variant="ghost"
          onclick={() => refreshProvider(provider)}
          disabled={!provider.canRefresh}
          aria-label={m.allowances_refresh_provider({ provider: provider.target.provider_name })}>
          {#if refreshingProvider}<Spinner
              data-icon="inline-start"
              aria-label={m.allowances_loading()} />{:else}<RefreshCwIcon />{/if}
        </Button>
      </div>
      {#if hasAlerts}
        <div class="grid gap-2 px-2 pb-3 @md:ps-8">{@render providerAlerts(provider)}</div>
      {/if}
      <!-- Collapsible.Content 收起时仍保留隐藏 DOM；只在展开时渲染详情，避免每行常驻一份明细与开关 -->
      {#if expanded}
        <Collapsible.Content class="border-t bg-muted/10 px-2 pb-2 @md:ps-8">
          {#if snapshot?.account_label}
            <dl class="flex flex-wrap items-baseline gap-x-2 py-2 text-xs text-muted-foreground">
              <dt>{m.allowances_detail_account()}</dt>
              <dd class="break-all" data-testid={`allowance-account-${providerId}`}>{snapshot.account_label}</dd>
            </dl>
          {/if}
          {#if provider.allowances.length > 0}
            {@render allowanceDetailTable(provider)}
          {/if}
          {#if snapshot && snapshot.models.length > 0}
            <Collapsible.Root class="mt-1">
              <Collapsible.Trigger
                class="inline-flex items-center gap-1 text-sm font-medium"
                aria-label={m.allowances_show_model_allowances({ provider: provider.target.provider_name })}>
                {m.allowances_model_allowances()}<ChevronDownIcon class="size-3.5" />
              </Collapsible.Trigger>
              <Collapsible.Content class="mt-2 max-w-2xl">{@render modelRows(snapshot.models)}</Collapsible.Content>
            </Collapsible.Root>
          {/if}
        </Collapsible.Content>
      {/if}
    </Collapsible.Root>
  </li>
{/snippet}

{#snippet compactAllowanceRows(allowances: Allowance[])}
  <div class="grid gap-2">
    {#each allowances as allowance (allowance.key)}
      <div class="grid gap-2 rounded-md border bg-muted/20 p-3 text-sm sm:grid-cols-[minmax(0,1fr)_auto_auto]">
        <span class="font-medium">{allowanceLabel(allowance)}</span>
        {@render allowanceValue(allowance, '')}
        <span class="font-technical text-muted-foreground tabular-nums">
          {allowance.reset_at != null ? m.allowances_reset_at({ time: resetDisplay(allowance) }) : '–'}
        </span>
      </div>
    {/each}
  </div>
{/snippet}

{#snippet modelRows(models: ModelAllowance[])}
  <Accordion.Root type="multiple" class="grid gap-2">
    {#each models as model (model.model)}
      <Accordion.Item value={model.model}>
        <Accordion.Trigger><span class="min-w-0 break-all">{model.model}</span></Accordion.Trigger>
        <Accordion.Content>{@render compactAllowanceRows(model.allowances)}</Accordion.Content>
      </Accordion.Item>
    {/each}
  </Accordion.Root>
{/snippet}

<div
  class={[
    'allowances-page route-page',
    { 'route-page-empty': !readState.loading && !readState.loadError && !hasEntries },
  ]}>
  <PageHeader title={m.allowances_title()}>
    {#snippet actions()}
      {#if !readState.loading && hasEntries}
        <span class="font-technical text-xs text-muted-foreground tabular-nums">
          {latestFetchedAt
            ? m.allowances_last_updated({ time: formatLogTime(latestFetchedAt, localeState.current) })
            : m.allowances_never_updated()}
        </span>
      {/if}
      {#if hasEntries}
        <Button onclick={refreshAll} disabled={readState.refreshAllDisabled}>
          {#if readState.refreshingAll}<Spinner
              data-icon="inline-start"
              aria-label={m.allowances_loading()} />{:else}<RefreshCwIcon />{/if}
          {m.allowances_refresh_all()}
        </Button>
      {/if}
    {/snippet}
  </PageHeader>

  {#if readState.loading}
    <div class="grid min-w-0 gap-3" aria-label={m.allowances_loading()}>
      <div class="allowances-filters route-section grid gap-2">
        <Skeleton class="h-9 w-full" /><Skeleton class="h-9 w-full" /><Skeleton class="h-9 w-full" />
        <Skeleton class="h-9 w-full" />
      </div>
      <Skeleton class="h-7 w-72" />
      <div class="allowances-split grid min-w-0 gap-3">
        <Skeleton class="h-96 w-full" />
        <div class="grid gap-3"><Skeleton class="h-40 w-full" /><Skeleton class="h-40 w-full" /></div>
      </div>
    </div>
  {:else if readState.loadError && !readState.hasTargets}
    <RequestFailure
      title={m.allowances_load_failed()}
      message={localizeBackendErrorMessage(readState.loadError)}
      retry={() => reading.retryTargets()}
      retrying={readState.fetching} />
  {:else if !hasEntries}
    <section class="route-section">
      <Empty.Root class="mx-auto my-auto w-full max-w-xl py-10"
        ><Empty.Header
          ><Empty.Media variant="icon"><GaugeIcon /></Empty.Media><Empty.Title role="heading" aria-level={2}
            >{m.allowances_empty_title()}</Empty.Title
          ><Empty.Description>{m.allowances_empty_description()}</Empty.Description></Empty.Header
        ><Empty.Content
          ><Button variant="outline" href="/providers">{m.allowances_manage_providers()}</Button></Empty.Content
        ></Empty.Root>
    </section>
  {:else}
    {#if readState.loadError}<RequestFailure
        title={m.allowances_stale_message()}
        message={localizeBackendErrorMessage(readState.loadError)}
        retry={() => reading.retryTargets()}
        retrying={readState.fetching} />{/if}

    <div class="grid min-w-0 gap-3">
      <section class="allowances-filters route-section grid items-center gap-2">
        <InputGroup.Root class="allowances-search min-w-0">
          <InputGroup.Input
            type="search"
            aria-label={m.allowances_search_label()}
            placeholder={m.allowances_search_placeholder()}
            bind:value={searchQuery} />
          <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
        </InputGroup.Root>
        <Select.Root type="single" bind:value={catalogFilter}>
          <Select.Trigger class="w-full justify-between">{catalogFilterLabel}</Select.Trigger>
          <Select.Content>
            <Select.Group>
              <Select.Item value="all">{m.allowances_filter_catalog_all()}</Select.Item>
              {#each catalogOptions as option (option.value)}
                <Select.Item value={option.value}>{option.label}</Select.Item>
              {/each}
            </Select.Group>
          </Select.Content>
        </Select.Root>
        <Select.Root type="single" bind:value={conditionFilter}>
          <Select.Trigger class="w-full justify-between">{conditionFilterLabel}</Select.Trigger>
          <Select.Content>
            <Select.Group>
              <Select.Item value="all">{m.allowances_filter_condition_all()}</Select.Item>
              <Select.Item value="normal">{m.allowances_condition_normal()}</Select.Item>
              <Select.Item value="tight">{m.allowances_condition_tight()}</Select.Item>
              <Select.Item value="exhausted">{m.allowances_condition_exhausted()}</Select.Item>
            </Select.Group>
          </Select.Content>
        </Select.Root>
        <Select.Root type="single" bind:value={freshnessFilter}>
          <Select.Trigger class="w-full justify-between">{freshnessFilterLabel}</Select.Trigger>
          <Select.Content>
            <Select.Group>
              <Select.Item value="all">{m.allowances_filter_freshness_all()}</Select.Item>
              <Select.Item value="fresh">{m.allowances_status_fresh()}</Select.Item>
              <Select.Item value="stale">{m.allowances_status_stale()}</Select.Item>
              <Select.Item value="error">{m.allowances_status_error()}</Select.Item>
            </Select.Group>
          </Select.Content>
        </Select.Root>
      </section>

      <section class="route-section" aria-labelledby="allowance-summary-title">
        <h2 id="allowance-summary-title" class="sr-only">{m.allowances_summary_title()}</h2>
        {#if !allResolved}
          <div class="flex items-center gap-2 text-sm text-muted-foreground">
            <Spinner aria-label={m.allowances_loading()} />
          </div>
        {:else}
          <div class="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
            <StatusIndicator
              compact
              label={conditionLabel(overallCondition)}
              tone={conditionStatusTone(overallCondition)} />
            <p class="font-technical tabular-nums">
              {lowestRemaining == null
                ? m.allowances_lowest_remaining({ value: '–' })
                : m.allowances_lowest_remaining({
                    value: formatAllowancePercent(lowestRemaining, localeState.current),
                  })}
            </p>
            <p class="font-technical tabular-nums">
              {nextResetAt == null
                ? m.allowances_no_upcoming_reset()
                : m.allowances_next_reset({ time: formatLogTime(nextResetAt, localeState.current) })}
            </p>
            {#if emptyWindows.length > 0}
              <Collapsible.Root open={exhaustedOpen} onOpenChange={(open: boolean) => (exhaustedOpen = open)}>
                <Collapsible.Trigger
                  class="inline-flex cursor-pointer items-center gap-1 rounded-md text-sm text-destructive transition-colors duration-[140ms] hover:underline"
                  aria-expanded={exhaustedOpen}>
                  <ChevronDownIcon
                    class={cn('size-3.5 transition-transform duration-[140ms]', exhaustedOpen && 'rotate-180')}
                    aria-hidden="true" />
                  {m.allowances_exhausted_windows({ count: emptyWindows.length })}
                </Collapsible.Trigger>
                {#if exhaustedOpen}
                  <Collapsible.Content>
                    <!-- 汇总只定位例外：明细按需展开，不复制矩阵内容 -->
                    <ul class="mt-1 grid gap-1 text-sm" data-testid="allowance-empty-windows">
                      {#each emptyWindows as item (`${item.snapshot.provider_id}:${item.allowance.key}`)}
                        <li>
                          {item.snapshot.provider_name} · {allowanceLabel(item.allowance)}
                          {#if item.allowance.reset_at != null}
                            <span class="font-technical text-muted-foreground tabular-nums">
                              · {m.allowances_reset_at({
                                time: formatLogTime(item.allowance.reset_at, localeState.current),
                              })}
                            </span>
                          {/if}
                        </li>
                      {/each}
                    </ul>
                  </Collapsible.Content>
                {/if}
              </Collapsible.Root>
            {/if}
          </div>
        {/if}
      </section>

      <div class="allowances-split grid min-w-0 gap-3">
        <section class="allowance-matrix-section route-section min-w-0" aria-labelledby="allowance-matrix-title">
          <div class="route-section-header items-center">
            <h2 id="allowance-matrix-title" class="route-section-title">{m.allowances_matrix_title()}</h2>
            <Tabs.Root bind:value={() => valueMode, changeValueMode}>
              <Tabs.List aria-label={m.allowances_value_mode()}>
                <Tabs.Trigger value="remaining">{m.allowances_remaining()}</Tabs.Trigger>
                <Tabs.Trigger value="used">{m.allowances_used()}</Tabs.Trigger>
              </Tabs.List>
            </Tabs.Root>
          </div>
          {#if visibleProviders.length === 0}
            <Empty.Root class="p-8"
              ><Empty.Header><Empty.Description>{m.allowances_filter_empty()}</Empty.Description></Empty.Header
              ></Empty.Root>
          {:else}
            <ul class="border-t" aria-label={m.allowances_matrix_title()}>
              {#each visibleProviders as provider (provider.target.provider_id)}
                {@render providerRow(provider)}
              {/each}
            </ul>
          {/if}
        </section>

        <div class="allowances-rail grid min-w-0 gap-3">
          <section class="route-section" aria-labelledby="allowance-timeline-title">
            <h2 id="allowance-timeline-title" class="route-section-title">{m.allowances_timeline_title()}</h2>
            <div id="allowance-reset-timeline">
              {#if !allResolved}
                <div class="flex items-center gap-2">
                  <Spinner aria-label={m.allowances_loading()} />
                </div>
              {:else if futureTimeline.length === 0}
                <p class="text-sm text-muted-foreground">{m.allowances_timeline_empty()}</p>
              {:else if displayedTimeline.length === 0}
                <p class="text-sm text-muted-foreground">{m.allowances_timeline_preview_empty()}</p>
              {:else}
                <ol class="relative ml-2 border-l" aria-label={m.allowances_timeline_title()}>
                  {#each displayedTimeline as item (`${item.snapshot.provider_id}:${item.allowance.key}`)}
                    {@const id = allowanceElementId(item.snapshot.provider_id, item.allowance.key)}
                    <li class="relative pb-1 pl-3 last:pb-0">
                      <span
                        class="absolute -left-1.5 top-3 size-3 rounded-full border-2 border-background bg-primary"
                        aria-hidden="true"></span>
                      <Button
                        variant="ghost"
                        class="w-full justify-start px-2 py-2 text-start whitespace-normal"
                        aria-controls={id}
                        aria-label={m.allowances_timeline_locate({
                          provider: item.snapshot.provider_name,
                          item: allowanceLabel(item.allowance),
                          time: formatLogTime(item.allowance.reset_at, localeState.current),
                        })}
                        onpointerenter={(event: PointerEvent) => {
                          if (event.pointerType !== 'touch') hoveredAllowanceId = id
                        }}
                        onpointerleave={() => (hoveredAllowanceId = null)}
                        onfocus={() => (focusedAllowanceId = id)}
                        onblur={() => (focusedAllowanceId = null)}
                        onclick={() => locateAllowance(id)}>
                        <span class="grid min-w-0 gap-1">
                          <span class="font-technical text-sm font-medium tabular-nums">
                            {formatLogTime(item.allowance.reset_at, localeState.current)}
                          </span>
                          <span class="text-sm text-muted-foreground">
                            {item.snapshot.provider_name} · {allowanceLabel(item.allowance)}
                          </span>
                        </span>
                      </Button>
                    </li>
                  {/each}
                </ol>
              {/if}
              {#if allResolved && futureTimeline.length > timelinePreview.length}
                <Button
                  variant="ghost"
                  class="mt-2 w-full"
                  aria-expanded={timelineExpanded}
                  aria-controls="allowance-reset-timeline"
                  onclick={() => (timelineExpanded = !timelineExpanded)}>
                  {#if timelineExpanded}
                    <ChevronRightIcon data-icon="inline-start" class="-rotate-90" />
                    {m.allowances_timeline_collapse()}
                  {:else}
                    <ChevronDownIcon data-icon="inline-start" />
                    {m.allowances_timeline_expand()}
                  {/if}
                </Button>
              {/if}
            </div>
          </section>

          <section class="route-section" aria-labelledby="allowance-forecast-title">
            <div class="flex items-center gap-1.5">
              <h2 id="allowance-forecast-title" class="route-section-title">{m.allowances_forecast_title()}</h2>
              <Field.Help hint={m.allowances_forecast_basis()} />
            </div>
            {#if !allResolved}
              <div class="flex items-center gap-2">
                <Spinner aria-label={m.allowances_loading()} />
              </div>
            {:else}
              <!-- 预报三张小卡改为紧凑计数行；截断只属于时间轴，这里统计全部可见额度 -->
              <ul class="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-sm">
                <li
                  class="font-technical tabular-nums"
                  data-testid="allowance-forecast-exhausted"
                  aria-label={m.allowances_forecast_exhausted_count({ count: forecastSummary.exhausted })}>
                  <span class={cn(forecastSummary.exhausted > 0 && 'font-semibold text-destructive')}>
                    {forecastSummary.exhausted}
                  </span>
                  <span class="ms-1 text-muted-foreground">{m.allowances_forecast_exhausted()}</span>
                </li>
                <li
                  class="font-technical tabular-nums"
                  data-testid="allowance-forecast-will-exhaust"
                  aria-label={m.allowances_forecast_will_exhaust_count({ count: forecastSummary.willExhaust })}>
                  <span class={cn(forecastSummary.willExhaust > 0 && 'font-semibold text-warning')}>
                    {forecastSummary.willExhaust}
                  </span>
                  <span class="ms-1 text-muted-foreground">{m.allowances_forecast_will_exhaust()}</span>
                </li>
                <li
                  class="font-technical tabular-nums"
                  data-testid="allowance-forecast-no-risk"
                  aria-label={m.allowances_forecast_no_risk_count({ count: forecastSummary.noRisk })}>
                  <span>{forecastSummary.noRisk}</span>
                  <span class="ms-1 text-muted-foreground">{m.allowances_forecast_no_risk()}</span>
                </li>
                {#if forecastSummary.unknown > 0}
                  <li class="text-xs text-muted-foreground" data-testid="allowance-forecast-unknown">
                    {m.allowances_forecast_unknown_count({ count: forecastSummary.unknown })}
                  </li>
                {/if}
              </ul>
              <p class="font-technical mt-2 text-sm tabular-nums">
                {forecastSummary.lowestProjected != null
                  ? m.allowances_forecast_lowest({
                      value: formatAllowancePercent(forecastSummary.lowestProjected, localeState.current),
                    })
                  : forecastSummary.exhausted > 0
                    ? m.allowances_forecast_windows_exhausted()
                    : m.allowances_forecast_no_projection()}
              </p>
              {#if forecastSummary.exhaustedItems.length > 0 || forecastSummary.willExhaustItems.length > 0}
                {#if willExhaustOpen}
                  <ul class="mt-2 grid gap-1 text-sm" data-testid="allowance-forecast-items">
                    {#each [...forecastSummary.exhaustedItems, ...forecastSummary.willExhaustItems] as item (`${item.snapshot.provider_id}:${item.allowance.key}`)}
                      <li>{forecastItemCopy(item)}</li>
                    {/each}
                  </ul>
                {/if}
                <button
                  type="button"
                  class="mt-1 inline-flex cursor-pointer items-center gap-1 rounded-md text-sm text-muted-foreground transition-colors duration-[140ms] hover:text-foreground hover:underline"
                  aria-expanded={willExhaustOpen}
                  onclick={() => (willExhaustOpen = !willExhaustOpen)}>
                  {#if willExhaustOpen}
                    {m.allowances_forecast_collapse()}
                  {:else}
                    {m.allowances_forecast_show_items({
                      count: forecastSummary.exhaustedItems.length + forecastSummary.willExhaustItems.length,
                    })}
                  {/if}
                </button>
              {/if}
            {/if}
          </section>
        </div>
      </div>
    </div>
  {/if}
</div>

<style>
/* 并排结构按工作区（route-page）宽度切换，不按 viewport。 */
.allowances-filters {
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
}

.allowances-filters > :global(.allowances-search) {
  grid-column: span 2;
}

@container route-page (min-width: 48rem) {
  .allowances-filters {
    grid-template-columns: minmax(12rem, 24rem) repeat(3, minmax(10rem, 15rem));
  }

  .allowances-filters > :global(.allowances-search) {
    grid-column: auto;
  }
}

@container route-page (min-width: 64rem) {
  /* 矩阵取剩余宽度，侧栏有界 24–28rem；不再沿用 2:1 拉伸。 */
  .allowances-split {
    grid-template-columns: minmax(0, 1fr) clamp(24rem, 28%, 28rem);
    align-items: start;
    column-gap: 2rem;
  }
}
</style>
