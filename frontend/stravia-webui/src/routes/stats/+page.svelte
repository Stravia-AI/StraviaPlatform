<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { BarChart, LineChart } from 'layerchart'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import {
  formatCompactCount,
  formatDuration,
  formatDurationSeconds,
  formatList,
  formatLogTime,
  formatPercent,
  formatTime,
} from '$lib/format'
import type { ApiKeyStats, ProviderStats } from '$lib/types'
import MetricStrip from '$lib/components/metric-strip.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import { Button } from '$lib/components/ui/button'
import { DataTable, createDataTableColumnHelper } from '$lib/components/ui/data-table'
import * as Select from '$lib/components/ui/select'
import { Skeleton } from '$lib/components/ui/skeleton'

let hours = $state('24')
const hoursNumber = $derived(Number(hours))
const overviewQuery = createQuery(() => ({
  queryKey: ['stats-overview', hoursNumber],
  queryFn: () => admin.stats.overview(hoursNumber),
  refetchInterval: 10_000,
}))
const hourlyQuery = createQuery(() => ({
  queryKey: ['stats-hourly', hoursNumber],
  queryFn: () => admin.stats.hourly(hoursNumber),
  refetchInterval: 30_000,
}))
const providersQuery = createQuery(() => ({
  queryKey: ['stats-providers', hoursNumber],
  queryFn: () => admin.stats.providers(hoursNumber),
  refetchInterval: 30_000,
}))
const apiKeysQuery = createQuery(() => ({
  queryKey: ['stats-api-keys', hoursNumber],
  queryFn: () => admin.stats.apiKeys(hoursNumber),
  refetchInterval: 30_000,
}))
const modelsQuery = createQuery(() => ({
  queryKey: ['stats-models', hoursNumber],
  queryFn: () => admin.stats.models(hoursNumber),
  refetchInterval: 30_000,
}))

const overview = $derived(overviewQuery.data)
const hasTraffic = $derived((overview?.total_requests ?? 0) > 0)
const providerStats = $derived(providersQuery.data ?? [])
const apiKeyStats = $derived(apiKeysQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const providerStatsColumnHelper = createDataTableColumnHelper<ProviderStats>()
const providerStatsColumns = providerStatsColumnHelper.columns([
  providerStatsColumnHelper.accessor('provider', {
    header: () => m.common_model_service(),
    meta: { label: () => m.common_model_service(), cellClass: 'font-medium' },
  }),
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
  providerStatsColumnHelper.accessor('avg_duration_ms', {
    header: () => m.common_avg_latency(),
    cell: (context) => formatDuration(context.getValue()),
    meta: { label: () => m.common_avg_latency(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
])
const apiKeyStatsColumnHelper = createDataTableColumnHelper<ApiKeyStats>()
const apiKeyStatsColumns = apiKeyStatsColumnHelper.columns([
  apiKeyStatsColumnHelper.accessor((apiKey) => apiKey.api_key_name || apiKey.api_key_id, {
    id: 'apiKey',
    header: () => m.common_api_key(),
    meta: { label: () => m.common_api_key(), cellClass: 'font-medium' },
  }),
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
    header: () => m.logs_cache_input_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.logs_cache_input_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('cache_write_tokens', {
    header: () => m.logs_cache_output_tokens(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.logs_cache_output_tokens(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  apiKeyStatsColumnHelper.accessor('last_used_at', {
    header: () => m.stats_last_used(),
    cell: (context) => formatLogTime(context.getValue()),
    meta: { label: () => m.stats_last_used(), align: 'end', cellClass: 'font-technical text-xs text-muted-foreground' },
  }),
])
const modelStats = $derived(modelsQuery.data ?? [])
const hourlyStats = $derived(hourlyQuery.data ?? [])
const tokenChart = $derived(
  hourlyStats.map((item) => ({
    bucket: formatBucket(item.hour),
    input: item.total_input_tokens,
    output: item.total_output_tokens,
    cacheInput: item.total_cache_read_tokens,
    cacheOutput: item.total_cache_write_tokens,
  })),
)
const latencyChart = $derived(
  hourlyStats.map((item) => ({
    bucket: formatBucket(item.hour),
    firstToken: item.avg_first_token_ms == null ? null : item.avg_first_token_ms / 1000,
    duration: item.avg_duration_ms / 1000,
  })),
)
const errorChart = $derived(hourlyStats.map((item) => ({ bucket: formatBucket(item.hour), errors: item.error_count })))
const modelTotal = $derived(modelStats.slice(0, 6).reduce((total, item) => total + item.request_count, 0))
const metrics = $derived([
  { label: m.common_total_requests(), value: formatCompactCount(overview?.total_requests ?? 0) },
  { label: m.stats_input_tokens(), value: formatCompactCount(overview?.total_input_tokens ?? 0) },
  { label: m.stats_output_tokens(), value: formatCompactCount(overview?.total_output_tokens ?? 0) },
  { label: m.logs_cache_input_tokens(), value: formatCompactCount(overview?.total_cache_read_tokens ?? 0) },
  { label: m.logs_cache_output_tokens(), value: formatCompactCount(overview?.total_cache_write_tokens ?? 0) },
  { label: m.common_avg_latency(), value: formatDuration(overview?.avg_duration_ms ?? 0) },
])
const anyError = $derived(
  overviewQuery.error ?? hourlyQuery.error ?? providersQuery.error ?? apiKeysQuery.error ?? modelsQuery.error,
)
const analyticsPending = $derived(
  overviewQuery.isPending ||
    hourlyQuery.isPending ||
    providersQuery.isPending ||
    apiKeysQuery.isPending ||
    modelsQuery.isPending,
)
const analyticsFetching = $derived(
  overviewQuery.isFetching ||
    hourlyQuery.isFetching ||
    providersQuery.isFetching ||
    apiKeysQuery.isFetching ||
    modelsQuery.isFetching,
)
const failedAnalyticsLabels = $derived.by(() => {
  const labels: string[] = []
  if (overviewQuery.error) labels.push(m.stats_summary())
  if (hourlyQuery.error) labels.push(m.stats_time_series())
  if (modelsQuery.error) labels.push(m.common_models())
  if (providersQuery.error) labels.push(m.common_model_services())
  if (apiKeysQuery.error) labels.push(m.app_shell_nav_api_keys())
  return labels
})

function getProviderStatsRowId(provider: ProviderStats): string {
  return provider.provider
}

function getApiKeyStatsRowId(apiKey: ApiKeyStats): string {
  return apiKey.api_key_id
}

function formatBucket(value: string): string {
  return hoursNumber <= 24 ? formatTime(value) : formatLogTime(value)
}

function retryAll(): void {
  void Promise.all([
    overviewQuery.refetch(),
    hourlyQuery.refetch(),
    providersQuery.refetch(),
    apiKeysQuery.refetch(),
    modelsQuery.refetch(),
  ])
}
</script>

<svelte:head><title>{m.stats_usage()} · Stravia</title></svelte:head>

{#snippet rangeAction()}
  <Select.Root type="single" bind:value={hours}>
    <Select.Trigger class="w-40" aria-label={m.stats_select_time_range()}
      >{hours === '6'
        ? m.stats_last_6h()
        : hours === '24'
          ? m.stats_last_24h()
          : hours === '72'
            ? m.stats_last_3d()
            : m.stats_last_7d()}</Select.Trigger>
    <Select.Content
      ><Select.Group
        ><Select.Item value="6">{m.stats_last_6h()}</Select.Item><Select.Item value="24"
          >{m.stats_last_24h()}</Select.Item
        ><Select.Item value="72">{m.stats_last_3d()}</Select.Item><Select.Item value="168"
          >{m.stats_last_7d()}</Select.Item
        ></Select.Group
      ></Select.Content>
  </Select.Root>
{/snippet}

{#snippet providerStatsEmpty()}
  <p class="py-6 text-center text-sm text-muted-foreground">{m.stats_no_model_service_traffic()}</p>
{/snippet}

{#snippet apiKeyStatsEmpty()}
  <p class="py-6 text-center text-sm text-muted-foreground">{m.stats_no_api_key_traffic()}</p>
{/snippet}

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={analyticsFetching ? m.stats_updating_10_30s() : m.stats_live_10_30s()}
    tone={anyError ? 'error' : 'healthy'} />
{/snippet}

{#snippet queryFailure(error: unknown, retry: () => unknown)}
  <div class="border-y py-6 text-center" role="alert">
    <p class="text-sm text-destructive">
      {localizeBackendErrorMessage(error)}
    </p>
    <Button class="mt-3" variant="outline" size="sm" onclick={() => void retry()}>{m.common_retry()}</Button>
  </div>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_monitor()}
    title={m.stats_usage()}
    description={m.stats_page_summary()}
    meta={liveMeta}
    actions={rangeAction} />

  {#if analyticsPending}
    <div class="route-metric-strip" aria-label={m.stats_loading_analytics_metrics()}>
      {#each Array(6) as _, index (index)}<div class="route-metric-strip__item">
          <Skeleton class="h-4 w-24" /><Skeleton class="mt-2 h-7 w-20" />
        </div>{/each}
    </div>
    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <Skeleton class="h-96 min-[1280px]:col-span-7" /><Skeleton class="h-96 min-[1280px]:col-span-5" />
    </div>
  {:else}
    {#if anyError}
      <section class="route-section" role="alert">
        <h2 class="route-section-title">
          {m.stats_some_usage_data_not_refreshed()}
        </h2>
        <p class="route-section-description text-destructive">
          {m.stats_refresh_failed({ labels: formatList(failedAnalyticsLabels) })}
        </p>
        <p class="mt-1 text-sm text-destructive">
          {localizeBackendErrorMessage(anyError)}
        </p>
        <p class="route-section-description">
          {m.stats_refresh_error_help()}
        </p>
        <Button class="mt-3" variant="outline" onclick={retryAll}>{m.stats_retry_all()}</Button>
      </section>
    {/if}

    {#if overviewQuery.error && overviewQuery.data === undefined}
      <section class="route-section" aria-labelledby="analytics-summary-error">
        <h2 id="analytics-summary-error" class="route-section-title">
          {m.stats_usage_summary_unavailable()}
        </h2>
        {@render queryFailure(overviewQuery.error, overviewQuery.refetch)}
      </section>
    {:else}
      <MetricStrip {metrics} label={m.common_usage_summary()} />
    {/if}

    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <section class="route-section flex flex-col min-[1280px]:col-span-7" aria-labelledby="token-trend-title">
        <div class="route-section-header">
          <div>
            <h2 id="token-trend-title" class="route-section-title">
              {m.stats_token_usage_over_time()}
            </h2>
            <p class="route-section-description">
              {m.stats_input_output_totals_each_time_period()}
            </p>
          </div>
        </div>
        {#if hourlyQuery.error && hourlyQuery.data === undefined}
          {@render queryFailure(hourlyQuery.error, hourlyQuery.refetch)}
        {:else if hasTraffic && tokenChart.length > 0}<div
            class="min-h-80 min-w-0 flex-1"
            aria-label={m.stats_token_usage_chart()}>
            <BarChart
              data={tokenChart}
              x={(item) => item.bucket}
              series={[
                { key: 'input', label: m.stats_input(), color: 'var(--chart-1)' },
                { key: 'output', label: m.stats_output(), color: 'var(--chart-3)' },
                { key: 'cacheInput', label: m.logs_cache_input_tokens(), color: 'var(--chart-2)' },
                { key: 'cacheOutput', label: m.logs_cache_output_tokens(), color: 'var(--chart-4)' },
              ]}
              seriesLayout="stack"
              props={{ xAxis: { ticks: 4 } }} />
          </div>{:else}<div class="grid min-h-80 flex-1 place-items-center border-y text-sm text-muted-foreground">
            {hasTraffic ? m.stats_no_token_usage_range() : m.stats_send_first_request()}
          </div>{/if}
      </section>

      <div class="grid gap-6 min-[1280px]:col-span-5">
        <section class="route-section" aria-labelledby="latency-trend-title">
          <div class="route-section-header">
            <div>
              <h2 id="latency-trend-title" class="route-section-title">{m.common_latency()}</h2>
              <p class="route-section-description">{m.stats_average_end_end_duration()}</p>
            </div>
            <div class="font-technical grid grid-cols-[auto_auto] gap-x-2 text-xs tabular-nums">
              <span class="text-muted-foreground">{m.logs_first_token_short()}</span>
              <span>{formatDurationSeconds(overview?.avg_first_token_ms)}</span>
              <span class="text-muted-foreground">{m.logs_duration_short()}</span>
              <span>{formatDurationSeconds(overview?.avg_duration_ms)}</span>
            </div>
          </div>
          {#if hourlyQuery.error && hourlyQuery.data === undefined}
            {@render queryFailure(hourlyQuery.error, hourlyQuery.refetch)}
          {:else if hasTraffic && latencyChart.length > 0}<div
              class="h-36 min-w-0"
              aria-label={m.overview_latency_chart()}>
              <LineChart
                data={latencyChart}
                x={(item) => item.bucket}
                series={[
                  { key: 'firstToken', label: m.stats_first_token_seconds(), color: 'var(--chart-2)' },
                  { key: 'duration', label: m.stats_duration_seconds(), color: 'var(--chart-1)' },
                ]}
                props={{ xAxis: { ticks: 4 } }} />
            </div>{:else}<div class="grid h-36 place-items-center border-y text-sm text-muted-foreground">
              {hasTraffic ? m.stats_no_latency_data() : m.stats_send_first_request()}
            </div>{/if}
        </section>
        <section class="route-section" aria-labelledby="error-trend-title">
          <div class="route-section-header">
            <div>
              <h2 id="error-trend-title" class="route-section-title">{m.common_errors_label()}</h2>
              <p class="route-section-description">
                {m.stats_failed_requests_time_bucket()}
              </p>
            </div>
            <span class="font-technical text-xs text-destructive tabular-nums">{overview?.error_count ?? 0}</span>
          </div>
          {#if hourlyQuery.error && hourlyQuery.data === undefined}
            {@render queryFailure(hourlyQuery.error, hourlyQuery.refetch)}
          {:else if hasTraffic && errorChart.length > 0}<div class="h-36 min-w-0">
              <BarChart
                data={errorChart}
                x={(item) => item.bucket}
                series={[{ key: 'errors', label: m.common_errors_label(), color: 'var(--chart-5)' }]}
                props={{ xAxis: { ticks: 4 } }} />
            </div>{:else}<div class="grid h-36 place-items-center border-y text-sm text-muted-foreground">
              {hasTraffic ? m.stats_no_error_data() : m.stats_send_first_request()}
            </div>{/if}
        </section>
      </div>
    </div>

    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <section class="route-section min-[1280px]:col-span-5" aria-labelledby="analytics-model-title">
        <div class="route-section-header">
          <div>
            <h2 id="analytics-model-title" class="route-section-title">{m.common_models()}</h2>
            <p class="route-section-description">{m.stats_share_all_requests()}</p>
          </div>
        </div>
        {#if modelsQuery.error && modelsQuery.data === undefined}
          {@render queryFailure(modelsQuery.error, modelsQuery.refetch)}
        {:else if modelStats.length > 0}<div class="flex flex-col gap-4">
            {#each modelStats.slice(0, 6) as model (model.model)}<div>
                <div class="mb-1 flex justify-between gap-3 text-sm">
                  <span class="font-technical truncate">{model.model}</span><span
                    class="font-technical text-muted-foreground tabular-nums"
                    >{formatPercent(modelTotal > 0 ? model.request_count / modelTotal : 0)}</span>
                </div>
                <div class="h-1.5 overflow-hidden rounded-full bg-muted">
                  <div
                    class="h-full rounded-full bg-chart-1"
                    style:width={formatPercent(modelTotal > 0 ? model.request_count / modelTotal : 0)}>
                  </div>
                </div>
              </div>{/each}
          </div>{:else}<p class="border-y py-8 text-center text-sm text-muted-foreground">
            {m.stats_no_model_traffic()}
          </p>{/if}
      </section>

      <section class="route-section min-[1280px]:col-span-7" aria-labelledby="analytics-provider-title">
        <div class="route-section-header">
          <div>
            <h2 id="analytics-provider-title" class="route-section-title">
              {m.common_model_services()}
            </h2>
            <p class="route-section-description">
              {m.stats_requests_errors_average_response_time_each_service()}
            </p>
          </div>
        </div>
        {#if providersQuery.error && providersQuery.data === undefined}
          {@render queryFailure(providersQuery.error, providersQuery.refetch)}
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
            {#if providerStats.length === 0}<p class="border-y py-8 text-center text-sm text-muted-foreground">
                {m.stats_no_model_service_traffic()}
              </p>{:else}{#each providerStats.slice(0, 8) as provider (provider.provider)}<div class="route-mobile-row">
                  <div class="min-w-0">
                    <p class="truncate font-medium">{provider.provider}</p>
                    <p class="font-technical mt-1 text-xs text-muted-foreground">
                      {formatDuration(provider.avg_duration_ms)} ·
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
        {@render queryFailure(apiKeysQuery.error, apiKeysQuery.refetch)}
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
          {#if apiKeyStats.length === 0}<p class="border-y py-8 text-center text-sm text-muted-foreground">
              {m.stats_no_api_key_traffic()}
            </p>{:else}{#each apiKeyStats.slice(0, 8) as apiKey (apiKey.api_key_id)}<div class="route-mobile-row">
                <div class="min-w-0">
                  <p class="truncate font-medium">{apiKey.api_key_name || apiKey.api_key_id}</p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    IN {formatCompactCount(apiKey.total_input_tokens)} · OUT {formatCompactCount(
                      apiKey.total_output_tokens,
                    )} · CACHE {formatCompactCount(apiKey.cache_read_tokens)}
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
