<script lang="ts">
import { onMount } from 'svelte'
import {
  liveStatsWindow,
  localStatsDateTime,
  parseLocalStatsDateTime,
  statsPresentationBucket,
} from '#lib/stats-window.js'
import type { StatsWindow } from '#lib/types/stats.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { createQuery } from '@tanstack/svelte-query'
import { BarChart, PieChart } from 'layerchart'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import {
  formatCompactCount,
  formatDuration,
  formatList,
  formatLogTime,
  formatPercent,
  formatTime,
  formatTps,
} from '#lib/format.js'
import { buildActivityGrid, buildLatencyChart, planActivityGrid } from '#lib/stats-chart.js'
import type { ApiKeyStats, ProviderStats } from '#lib/types.js'
import MetricStrip from '#lib/components/metric-strip.svelte'
import LatencySpeedChart from '#lib/components/latency-speed-chart.svelte'
import LatencySpeedSummary from '#lib/components/latency-speed-summary.svelte'
import TokenActivityGrid from '#lib/components/token-activity-grid.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import { DataTable, createDataTableColumnHelper } from '#lib/components/ui/data-table/index.js'
import TimeWindowControls from '#lib/components/time-window-controls.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'

let hours = $state('24')
const HOUR_MS = 3_600_000
let spanMs = $state(24 * HOUR_MS)
let live = $state(true)
let window = $state.raw<StatsWindow>(liveStatsWindow(24 * HOUR_MS))
let rangeOpen = $state(false)
let draftStart = $state('')
let draftEnd = $state('')
const draftStartMs = $derived(parseLocalStatsDateTime(draftStart))
const draftEndMs = $derived(parseLocalStatsDateTime(draftEnd))
const validDraftRange = $derived(
  draftStartMs != null &&
    draftEndMs != null &&
    draftEndMs > draftStartMs &&
    draftEndMs <= liveStatsWindow(spanMs).end_at,
)
const hoursNumber = $derived(spanMs / HOUR_MS)
const seriesBucketSeconds = $derived(statsPresentationBucket(spanMs, 3_600))
const activityBucketSeconds = $derived(
  statsPresentationBucket(
    spanMs,
    hoursNumber <= 6 ? 900 : hoursNumber <= 24 ? 3_600 : hoursNumber <= 72 ? 21_600 : 86_400,
  ),
)
let activityWidth = $state(0)
let activityHeight = $state(0)
const activityPlan = $derived(
  planActivityGrid({
    startMs: window.start_at,
    endMs: window.end_at,
    bucketMs: activityBucketSeconds * 1_000,
    widthPx: activityWidth,
    heightPx: activityHeight,
  }),
)
// Every query key owns an immutable absolute interval. Late responses only update their old key;
// no placeholder data from another selection is presented as the newly selected window.
const overviewQuery = createQuery(() => ({
  queryKey: ['stats-overview', window.start_at, window.end_at],
  queryFn: ({ queryKey }) => admin.stats.overview({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }),
  refetchInterval: 30_000,
}))
const seriesQuery = createQuery(() => ({
  queryKey: ['stats-series', window.start_at, window.end_at, seriesBucketSeconds],
  queryFn: ({ queryKey }) =>
    admin.stats.series({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }, Number(queryKey[3])),
  refetchInterval: 30_000,
}))
const activityQuery = createQuery(() => ({
  queryKey: ['stats-series', activityPlan.startMs, activityPlan.endMs, activityPlan.bucketMs / 1_000],
  queryFn: ({ queryKey }) =>
    admin.stats.series({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }, Number(queryKey[3])),
  enabled: activityWidth > 0 && activityHeight > 0,
  refetchInterval: 30_000,
}))
const providersQuery = createQuery(() => ({
  queryKey: ['stats-providers', window.start_at, window.end_at],
  queryFn: ({ queryKey }) => admin.stats.providers({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }),
  refetchInterval: 30_000,
}))
const apiKeysQuery = createQuery(() => ({
  queryKey: ['stats-api-keys', window.start_at, window.end_at],
  queryFn: ({ queryKey }) => admin.stats.apiKeys({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }),
  refetchInterval: 30_000,
}))
const modelsQuery = createQuery(() => ({
  queryKey: ['stats-models', window.start_at, window.end_at],
  queryFn: ({ queryKey }) => admin.stats.models({ start_at: Number(queryKey[1]), end_at: Number(queryKey[2]) }),
  refetchInterval: 30_000,
}))
onMount(() => {
  const timer = setInterval(() => {
    if (!live) return
    const current = liveStatsWindow(spanMs)
    if (current.end_at !== window.end_at) window = current
  }, 30_000)
  return () => clearInterval(timer)
})
function selectPreset(value: string): void {
  hours = value
  spanMs = Number(value) * HOUR_MS
  live = true
  window = liveStatsWindow(spanMs)
}
function shiftWindow(direction: -1 | 1): void {
  const end_at = window.end_at + direction * spanMs
  if (end_at > liveStatsWindow(spanMs).end_at) return
  live = false
  window = { start_at: window.start_at + direction * spanMs, end_at }
}
function returnLive(): void {
  live = true
  window = liveStatsWindow(spanMs)
}
function openRange(): void {
  draftStart = localStatsDateTime(window.start_at)
  draftEnd = localStatsDateTime(window.end_at)
  rangeOpen = true
}
function applyRange(): void {
  if (!validDraftRange || draftStartMs == null || draftEndMs == null) return
  spanMs = draftEndMs - draftStartMs
  hours = ''
  window = { start_at: draftStartMs, end_at: draftEndMs }
  live = false
  rangeOpen = false
}

const overview = $derived(overviewQuery.data)
const hasTraffic = $derived((overview?.total_requests ?? 0) > 0)
const providerStats = $derived(providersQuery.data ?? [])
const apiKeyStats = $derived(apiKeysQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const providerStatsColumnHelper = createDataTableColumnHelper<ProviderStats>()
const providerStatsColumns = providerStatsColumnHelper.columns([
  providerStatsColumnHelper.accessor(
    (provider) =>
      `${provider.provider || provider.provider_id || '—'}${provider.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`,
    {
      id: 'provider',
      header: () => m.common_model_service(),
      meta: { label: () => m.common_model_service(), cellClass: 'font-medium' },
    },
  ),
  providerStatsColumnHelper.accessor('request_count', {
    header: () => m.common_request_count_label(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.common_request_count_label(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  providerStatsColumnHelper.accessor('error_count', {
    header: () => m.common_error_count_label(),
    meta: {
      label: () => m.common_error_count_label(),
      align: 'end',
      cellClass: 'font-technical text-destructive tabular-nums',
    },
  }),
  providerStatsColumnHelper.accessor(
    (provider) => (provider.request_count > 0 ? provider.error_count / provider.request_count : 0),
    {
      id: 'errorRate',
      header: () => m.common_error_rate(),
      cell: (context) => formatPercent(context.getValue()),
      meta: { label: () => m.common_error_rate(), align: 'end', cellClass: 'font-technical tabular-nums' },
    },
  ),
  providerStatsColumnHelper.accessor('avg_output_tps', {
    header: () => m.stats_output_speed(),
    cell: (context) => formatTps(context.getValue()),
    meta: { label: () => m.stats_output_speed(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
])
const apiKeyStatsColumnHelper = createDataTableColumnHelper<ApiKeyStats>()
const apiKeyStatsColumns = apiKeyStatsColumnHelper.columns([
  apiKeyStatsColumnHelper.accessor(
    (apiKey) =>
      `${apiKey.api_key_name || apiKey.api_key_id}${apiKey.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`,
    {
      id: 'apiKey',
      header: () => m.common_api_key(),
      meta: { label: () => m.common_api_key(), cellClass: 'font-medium' },
    },
  ),
  apiKeyStatsColumnHelper.accessor('request_count', {
    header: () => m.common_request_count_label(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.common_request_count_label(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('total_input_tokens', {
    header: () => m.stats_input_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.stats_input_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('total_output_tokens', {
    header: () => m.stats_output_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.stats_output_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('cache_read_tokens', {
    header: () => m.stats_cache_read_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.stats_cache_read_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('cache_write_tokens', {
    header: () => m.stats_cache_write_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.stats_cache_write_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('last_used_at', {
    header: () => m.stats_last_used(),
    cell: (context) => formatLogTime(context.getValue()),
    meta: { label: () => m.stats_last_used(), align: 'end', cellClass: 'font-technical text-xs text-muted-foreground' },
  }),
])
const modelStats = $derived(modelsQuery.data ?? [])
const seriesStats = $derived(seriesQuery.data ?? [])
const activityGrid = $derived(
  buildActivityGrid(activityQuery.data ?? [], {
    startMs: activityPlan.startMs,
    endMs: activityPlan.endMs,
    bucketMs: activityQuery.data?.[0]?.bucket_ms ?? activityPlan.bucketMs,
  }),
)
const latencyChart = $derived(buildLatencyChart(seriesStats, seriesStats[0]?.bucket_ms ?? seriesBucketSeconds * 1_000))
const hasLatencyMetrics = $derived(latencyChart.some((point) => point.firstToken != null || point.outputTps != null))
const errorChart = $derived(
  seriesStats.map((item) => ({ bucket: formatBucket(item.bucket_start), errors: item.error_count })),
)
interface PieSlice {
  key: string
  label: string
  value: number
  color: string
}
const modelTotal = $derived(modelStats.reduce((total, item) => total + item.request_count, 0))
const MODEL_PIE_COLORS = ['var(--chart-1)', 'var(--chart-2)', 'var(--chart-3)', 'var(--chart-4)']
const modelPie = $derived.by((): PieSlice[] => {
  const slices = modelStats
    .slice(0, 4)
    .map((item, index) => ({
      key: item.model_id ?? item.model,
      label: `${item.model || item.model_id || '—'}${item.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`,
      value: item.request_count,
      color: MODEL_PIE_COLORS[index],
    }))
  const rest = modelStats.slice(4).reduce((total, item) => total + item.request_count, 0)
  if (rest > 0) slices.push({ key: '__other__', label: m.stats_other(), value: rest, color: 'var(--muted)' })
  return slices
})
const TOKEN_PIE_COLORS = ['var(--chart-1)', 'var(--chart-2)']
// 未上报的类别不计入占比，避免把"未报告"显示成 0%。
const tokenPie = $derived.by((): { slices: PieSlice[]; total: number } | null => {
  if (overview == null) return null
  const categories = [
    { key: 'input', label: m.stats_input_tokens(), value: overview.total_input_tokens },
    { key: 'output', label: m.stats_output_tokens(), value: overview.total_output_tokens },
  ]
  const slices = categories.flatMap((category, index) =>
    category.value == null
      ? []
      : [{ key: category.key, label: category.label, value: category.value, color: TOKEN_PIE_COLORS[index] }],
  )
  const total = slices.reduce((sum, slice) => sum + slice.value, 0)
  return total > 0 ? { slices, total } : null
})
const metrics = $derived([
  { label: m.common_total_requests(), value: formatCompactCount(overview?.total_requests) },
  { label: m.stats_input_tokens(), value: formatCompactCount(overview?.total_input_tokens) },
  { label: m.stats_output_tokens(), value: formatCompactCount(overview?.total_output_tokens) },
  { label: m.stats_cache_read_tokens(), value: formatCompactCount(overview?.total_cache_read_tokens) },
  { label: m.stats_cache_write_tokens(), value: formatCompactCount(overview?.total_cache_write_tokens) },
  { label: m.common_avg_latency(), value: formatDuration(overview?.avg_duration_ms) },
])
const anyError = $derived(
  overviewQuery.error ??
    seriesQuery.error ??
    activityQuery.error ??
    providersQuery.error ??
    apiKeysQuery.error ??
    modelsQuery.error,
)
// 先读取每个查询状态，避免短路跳过 TanStack 属性订阅后错过先完成的查询。
const analyticsPending = $derived.by(() => {
  const overview = overviewQuery.isPending
  const series = seriesQuery.isPending
  const providers = providersQuery.isPending
  const apiKeys = apiKeysQuery.isPending
  const models = modelsQuery.isPending
  return overview || series || providers || apiKeys || models
})
const analyticsFetching = $derived.by(() => {
  const overview = overviewQuery.isFetching
  const series = seriesQuery.isFetching
  const activity = activityQuery.isFetching
  const providers = providersQuery.isFetching
  const apiKeys = apiKeysQuery.isFetching
  const models = modelsQuery.isFetching
  return overview || series || activity || providers || apiKeys || models
})
const failedAnalyticsLabels = $derived.by(() => {
  const labels: string[] = []
  if (overviewQuery.error) labels.push(m.stats_summary())
  if (seriesQuery.error) labels.push(m.stats_time_series())
  if (activityQuery.error && activityQuery.error !== seriesQuery.error) labels.push(m.stats_token_activity())
  if (modelsQuery.error) labels.push(m.common_models())
  if (providersQuery.error) labels.push(m.common_model_services())
  if (apiKeysQuery.error) labels.push(m.app_shell_nav_api_keys())
  return labels
})

function getProviderStatsRowId(provider: ProviderStats): string {
  return provider.provider_id ?? provider.provider
}

function getApiKeyStatsRowId(apiKey: ApiKeyStats): string {
  return apiKey.api_key_id
}

function formatBucket(value: number | Date): string {
  return hoursNumber <= 24 ? formatTime(value) : formatLogTime(value)
}

function retryAll(): void {
  void Promise.all([
    overviewQuery.refetch(),
    seriesQuery.refetch(),
    activityQuery.refetch(),
    providersQuery.refetch(),
    apiKeysQuery.refetch(),
    modelsQuery.refetch(),
  ])
}
</script>

<svelte:head><title>{m.stats_usage()} · Stravia</title></svelte:head>

{#snippet rangeAction()}
  <TimeWindowControls
    presets={[
      { value: '6', label: m.stats_last_6h() },
      { value: '24', label: m.stats_last_24h() },
      { value: '72', label: m.stats_last_3d() },
      { value: '168', label: m.stats_last_7d() },
    ]}
    value={hours}
    selectedLabel={hours === '6'
      ? m.stats_last_6h()
      : hours === '24'
        ? m.stats_last_24h()
        : hours === '72'
          ? m.stats_last_3d()
          : hours === '168'
            ? m.stats_last_7d()
            : m.observation_custom_range()}
    startMs={window.start_at}
    endMs={window.end_at}
    {live}
    selectLabel={m.stats_select_time_range()}
    rangeLabel={m.stats_date_time_range()}
    olderLabel={m.stats_previous_window()}
    newerLabel={m.stats_next_window()}
    newerDisabled={window.end_at + spanMs > liveStatsWindow(spanMs).end_at}
    onpreset={selectPreset}
    onolder={() => shiftWindow(-1)}
    onnewer={() => shiftWindow(1)}
    onrange={openRange}
    trailing={liveAction} />
{/snippet}

{#snippet liveAction()}
  {#if !live}<Button variant="ghost" size="sm" onclick={returnLive}>{m.stats_return_live()}</Button>{/if}
{/snippet}

{#snippet providerStatsEmpty()}
  <Empty.Root class="border-y py-6"
    ><Empty.Header><Empty.Description>{m.stats_no_model_service_traffic()}</Empty.Description></Empty.Header
    ></Empty.Root>
{/snippet}

{#snippet apiKeyStatsEmpty()}
  <Empty.Root class="border-y py-6"
    ><Empty.Header><Empty.Description>{m.stats_no_api_key_traffic()}</Empty.Description></Empty.Header></Empty.Root>
{/snippet}

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={analyticsFetching ? m.stats_updating_30s() : live ? m.stats_live_30s() : m.stats_fixed_window()}
    tone={anyError ? 'error' : 'healthy'} />
{/snippet}

{#snippet queryFailure(error: unknown, retry: () => unknown, retrying: boolean)}
  <RequestFailure message={localizeBackendErrorMessage(error)} {retry} {retrying} />
{/snippet}

<div class="route-page">
  <PageHeader title={m.stats_usage_statistics()} meta={liveMeta} actions={rangeAction} />
  {#if analyticsPending}
    <MetricStrip loading loadingLabel={m.stats_loading_analytics_metrics()} placeholderCount={6} />
    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <Skeleton class="h-96 min-[1280px]:col-span-7" /><Skeleton class="h-96 min-[1280px]:col-span-5" />
    </div>
  {:else}
    {#if anyError}
      <RequestFailure title={m.stats_some_usage_data_not_refreshed()} message={localizeBackendErrorMessage(anyError)}>
        <p>{m.stats_refresh_failed({ labels: formatList(failedAnalyticsLabels) })}</p>
        <p>{m.stats_refresh_error_help()}</p>
        <Button type="button" variant="outline" disabled={analyticsFetching} onclick={retryAll}
          >{m.stats_retry_all()}</Button>
      </RequestFailure>
    {/if}

    {#if overviewQuery.error && overviewQuery.data === undefined}
      <section class="route-section" aria-labelledby="analytics-summary-error">
        <h2 id="analytics-summary-error" class="route-section-title">
          {m.stats_usage_summary_unavailable()}
        </h2>
        {@render queryFailure(overviewQuery.error, overviewQuery.refetch, overviewQuery.isFetching)}
      </section>
    {:else}
      <MetricStrip {metrics} label={m.common_usage_summary()} />
    {/if}

    <div class="grid gap-6 min-[1280px]:grid-cols-12 min-[1280px]:gap-y-0">
      <section
        class="route-section flex flex-col min-[1280px]:col-span-7 min-[1280px]:row-span-2 min-[1280px]:grid min-[1280px]:grid-rows-subgrid"
        aria-labelledby="token-activity-title">
        <div class="route-section-header">
          <div>
            <h2 id="token-activity-title" class="route-section-title">
              {m.stats_token_activity()}
            </h2>
            <p class="route-section-description">
              {m.stats_input_output_totals_each_time_period()}
            </p>
          </div>
        </div>
        <div
          class="h-40 min-w-0 flex flex-col justify-center py-2"
          bind:clientWidth={activityWidth}
          bind:clientHeight={activityHeight}
          data-activity-start={activityPlan.startMs}
          data-activity-end={activityPlan.endMs}
          data-activity-bucket={activityPlan.bucketMs}>
          {#if activityQuery.error && activityQuery.data === undefined}
            {@render queryFailure(activityQuery.error, activityQuery.refetch, activityQuery.isFetching)}
          {:else if activityWidth === 0 || activityHeight === 0 || activityQuery.isPending}
            <Skeleton class="h-full w-full" />
          {:else}
            <TokenActivityGrid model={activityGrid} cellPx={activityPlan.cellPx} />
          {/if}
        </div>
      </section>

      <section
        class="route-section min-[1280px]:col-span-5 min-[1280px]:row-span-2 min-[1280px]:grid min-[1280px]:grid-rows-subgrid"
        aria-labelledby="latency-trend-title">
        <div class="route-section-header">
          <div>
            <h2 id="latency-trend-title" class="route-section-title">{m.stats_latency_speed()}</h2>
            <p class="route-section-description">{m.stats_latency_speed_description()}</p>
          </div>
          <LatencySpeedSummary {overview} />
        </div>
        {#if seriesQuery.error && seriesQuery.data === undefined}
          {@render queryFailure(seriesQuery.error, seriesQuery.refetch, seriesQuery.isFetching)}
        {:else if seriesQuery.isPending}<Skeleton class="h-40" />
        {:else if hasLatencyMetrics}<div class="h-40 min-w-0">
            <LatencySpeedChart data={latencyChart} {formatBucket} />
          </div>{:else}<Empty.Root class="h-40 border-y"
            ><Empty.Header
              ><Empty.Description
                >{hasTraffic ? m.stats_no_latency_speed_data() : m.stats_send_first_request()}</Empty.Description
              ></Empty.Header
            ></Empty.Root
          >{/if}
      </section>
    </div>

    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <section class="route-section min-[1280px]:col-span-4" aria-labelledby="error-trend-title">
        <div class="route-section-header">
          <div>
            <h2 id="error-trend-title" class="route-section-title">{m.common_errors_label()}</h2>
            <p class="route-section-description">
              {m.stats_failed_requests_time_bucket()}
            </p>
          </div>
          <span class="font-technical text-xs text-destructive tabular-nums">{overview?.error_count ?? '–'}</span>
        </div>
        {#if seriesQuery.error && seriesQuery.data === undefined}
          {@render queryFailure(seriesQuery.error, seriesQuery.refetch, seriesQuery.isFetching)}
        {:else if hasTraffic && errorChart.length > 0}<div class="h-40 min-w-0">
            <BarChart
              data={errorChart}
              x={(item: (typeof errorChart)[number]) => item.bucket}
              series={[{ key: 'errors', label: m.common_errors_label(), color: 'var(--chart-5)' }]}
              props={{ xAxis: { ticks: 4 } }} />
          </div>{:else}<Empty.Root class="h-40 border-y"
            ><Empty.Header
              ><Empty.Description
                >{hasTraffic ? m.stats_no_error_data() : m.stats_send_first_request()}</Empty.Description
              ></Empty.Header
            ></Empty.Root
          >{/if}
      </section>

      <section class="route-section min-[1280px]:col-span-4" aria-labelledby="token-breakdown-title">
        <div class="route-section-header">
          <div>
            <h2 id="token-breakdown-title" class="route-section-title">{m.stats_token_breakdown()}</h2>
            <p class="route-section-description">{m.stats_token_breakdown_share()}</p>
          </div>
        </div>
        {#if overviewQuery.error && overview === undefined}
          {@render queryFailure(overviewQuery.error, overviewQuery.refetch, overviewQuery.isFetching)}
        {:else if tokenPie}<div class="flex flex-col items-center gap-4">
            <div class="relative size-40 min-w-0" aria-label={m.stats_token_breakdown()}>
              <PieChart
                data={tokenPie.slices}
                key="key"
                label="label"
                value="value"
                c="key"
                cDomain={tokenPie.slices.map((slice) => slice.key)}
                cRange={tokenPie.slices.map((slice) => slice.color)}
                innerRadius={0.68}
                cornerRadius={2}
                padAngle={0.01} />
              <div class="pointer-events-none absolute inset-0 grid place-items-center">
                <span class="font-technical text-2xl font-medium tabular-nums"
                  >{formatCompactCount(tokenPie.total)}</span>
              </div>
            </div>
            <ul class="w-full space-y-2 text-sm">
              {#each tokenPie.slices as slice (slice.key)}
                <li class="flex items-center gap-2">
                  <span class="size-2.5 shrink-0 rounded-[2px]" style:background={slice.color}></span>
                  <span class="truncate">{slice.label}</span>
                  <span class="font-technical ml-auto text-muted-foreground tabular-nums"
                    >{formatPercent(slice.value / tokenPie.total)}</span>
                </li>
              {/each}
            </ul>
          </div>{:else}<Empty.Root class="border-y py-6"
            ><Empty.Header
              ><Empty.Description
                >{hasTraffic
                  ? overview &&
                    [overview.total_input_tokens, overview.total_output_tokens].some((value) => value != null) &&
                    (overview.total_input_tokens ?? 0) + (overview.total_output_tokens ?? 0) === 0
                    ? m.stats_zero_tokens()
                    : m.stats_token_usage_unavailable()
                  : m.stats_send_first_request()}</Empty.Description
              ></Empty.Header
            ></Empty.Root
          >{/if}
      </section>

      <section class="route-section min-[1280px]:col-span-4" aria-labelledby="analytics-model-title">
        <div class="route-section-header">
          <div>
            <h2 id="analytics-model-title" class="route-section-title">{m.common_models()}</h2>
            <p class="route-section-description">{m.stats_share_all_requests()}</p>
          </div>
        </div>
        {#if modelsQuery.error && modelsQuery.data === undefined}
          {@render queryFailure(modelsQuery.error, modelsQuery.refetch, modelsQuery.isFetching)}
        {:else if modelPie.length > 0}<div class="flex flex-col items-center gap-4">
            <div class="size-40 min-w-0" aria-label={m.stats_share_all_requests()}>
              <PieChart
                data={modelPie}
                key="key"
                label="label"
                value="value"
                c="key"
                cDomain={modelPie.map((slice) => slice.key)}
                cRange={modelPie.map((slice) => slice.color)}
                innerRadius={0.68}
                cornerRadius={2}
                padAngle={0.01} />
            </div>
            <ul class="w-full space-y-2 text-sm">
              {#each modelPie as slice (slice.key)}
                <li class="flex items-center gap-2">
                  <span class="size-2.5 shrink-0 rounded-[2px]" style:background={slice.color}></span>
                  <span class="font-technical truncate">{slice.label}</span>
                  <span class="font-technical ml-auto text-muted-foreground tabular-nums"
                    >{formatPercent(modelTotal > 0 ? slice.value / modelTotal : 0)}</span>
                </li>
              {/each}
            </ul>
          </div>{:else}<Empty.Root class="border-y py-6"
            ><Empty.Header><Empty.Description>{m.stats_no_model_traffic()}</Empty.Description></Empty.Header
            ></Empty.Root
          >{/if}
      </section>
    </div>

    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <section class="route-section min-[1280px]:col-span-12" aria-labelledby="analytics-provider-title">
        <div class="route-section-header">
          <div>
            <h2 id="analytics-provider-title" class="route-section-title">
              {m.common_model_services()}
            </h2>
            <p class="route-section-description">
              {m.stats_requests_errors_output_speed_each_service()}
            </p>
          </div>
        </div>
        {#if providersQuery.error && providersQuery.data === undefined}
          {@render queryFailure(providersQuery.error, providersQuery.refetch, providersQuery.isFetching)}
        {:else}
          <div class="route-desktop-table">
            <DataTable
              data={providerStats.slice(0, 8)}
              columns={providerStatsColumns}
              labels={tableLabels}
              getRowId={getProviderStatsRowId}
              ariaLabel={m.common_model_services()}
              empty={providerStatsEmpty}
              stripedRows />
          </div>
          <div class="route-mobile-list">
            {#if providerStats.length === 0}<Empty.Root class="border-y py-6"
                ><Empty.Header><Empty.Description>{m.stats_no_model_service_traffic()}</Empty.Description></Empty.Header
                ></Empty.Root
              >{:else}{#each providerStats.slice(0, 8) as provider (provider.provider_id ?? provider.provider)}<div
                  class="route-mobile-row">
                  <div class="min-w-0">
                    <p class="truncate font-medium">
                      {provider.provider || provider.provider_id || '—'}{#if provider.deleted}
                        · {m.stats_deleted_resource()}{/if}
                    </p>
                    <p class="font-technical mt-1 text-xs text-muted-foreground">
                      {formatTps(provider.avg_output_tps)} ·
                      <span class="text-destructive">{provider.error_count} {m.common_errors()}</span>
                    </p>
                  </div>
                  <p class="font-technical tabular-nums">{formatCompactCount(provider.request_count)}</p>
                </div>{/each}{/if}
          </div>
        {/if}
      </section>
    </div>

    <section class="route-section" aria-labelledby="analytics-key-title">
      <div class="route-section-header">
        <div>
          <h2 id="analytics-key-title" class="route-section-title">{m.stats_api_key_usage()}</h2>
          <p class="route-section-description">
            {m.stats_client_volume_token_usage_cache_reads_last_activity()}
          </p>
        </div>
      </div>
      {#if apiKeysQuery.error && apiKeysQuery.data === undefined}
        {@render queryFailure(apiKeysQuery.error, apiKeysQuery.refetch, apiKeysQuery.isFetching)}
      {:else}
        <div class="route-desktop-table">
          <DataTable
            data={apiKeyStats.slice(0, 8)}
            columns={apiKeyStatsColumns}
            labels={tableLabels}
            getRowId={getApiKeyStatsRowId}
            ariaLabel={m.stats_api_key_usage()}
            empty={apiKeyStatsEmpty}
            stripedRows />
        </div>
        <div class="route-mobile-list">
          {#if apiKeyStats.length === 0}<Empty.Root class="border-y py-6"
              ><Empty.Header><Empty.Description>{m.stats_no_api_key_traffic()}</Empty.Description></Empty.Header
              ></Empty.Root
            >{:else}{#each apiKeyStats.slice(0, 8) as apiKey (apiKey.api_key_id)}<div class="route-mobile-row">
                <div class="min-w-0">
                  <p class="truncate font-medium">
                    {apiKey.api_key_name || apiKey.api_key_id}{#if apiKey.deleted}
                      · {m.stats_deleted_resource()}{/if}
                  </p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    {m.observation_usage_input()}
                    {formatCompactCount(apiKey.total_input_tokens)} · {m.observation_usage_output()}
                    {formatCompactCount(apiKey.total_output_tokens)} ·
                    {m.observation_usage_cache_read()}
                    {formatCompactCount(apiKey.cache_read_tokens)} · {m.observation_usage_cache_write()}
                    {formatCompactCount(apiKey.cache_write_tokens)}
                  </p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">{formatLogTime(apiKey.last_used_at)}</p>
                </div>
                <p class="font-technical tabular-nums">{formatCompactCount(apiKey.request_count)}</p>
              </div>{/each}{/if}
        </div>
      {/if}
    </section>
  {/if}
</div>

<Dialog.Root bind:open={rangeOpen}>
  <Dialog.Content>
    <Dialog.Header>
      <Dialog.Title>{m.stats_date_time_range()}</Dialog.Title>
      <Dialog.Description>{m.stats_range_help()}</Dialog.Description>
    </Dialog.Header>
    <form
      class="flex flex-col gap-4"
      onsubmit={(event) => {
        event.preventDefault()
        applyRange()
      }}>
      <Field.FieldGroup>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="stats-start">{m.stats_range_start()}</Field.FieldLabel>
          <Input
            id="stats-start"
            type="datetime-local"
            step="900"
            required
            bind:value={draftStart}
            aria-invalid={!validDraftRange}
            aria-describedby="stats-range-validation" />
        </Field.Field>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="stats-end">{m.stats_range_end()}</Field.FieldLabel>
          <Input
            id="stats-end"
            type="datetime-local"
            step="900"
            required
            bind:value={draftEnd}
            aria-invalid={!validDraftRange}
            aria-describedby="stats-range-validation" />
        </Field.Field>
      </Field.FieldGroup>
      <p
        id="stats-range-validation"
        class="text-sm"
        class:text-destructive={!validDraftRange}
        role={validDraftRange ? 'status' : 'alert'}>
        {validDraftRange ? m.stats_range_help() : m.stats_range_invalid()}
      </p>
      <Dialog.Footer>
        <Button type="button" variant="outline" onclick={() => (rangeOpen = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={!validDraftRange}>{m.stats_apply_range()}</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>
