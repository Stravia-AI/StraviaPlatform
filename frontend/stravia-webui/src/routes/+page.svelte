<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { BarChart, LineChart } from 'layerchart'

import { admin, isTauri } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import {
  formatCompactCount,
  formatDuration,
  formatDurationSeconds,
  formatNumber,
  formatPercent,
  formatTime,
} from '$lib/format'
import type { ModelStats, ProviderStats } from '$lib/types'
import DesktopPortNotice from '$lib/components/desktop-port-notice.svelte'
import MetricStrip from '$lib/components/metric-strip.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RouteSpine from '$lib/components/route-spine.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import { Button } from '$lib/components/ui/button'
import { DataTable, createDataTableColumnHelper } from '$lib/components/ui/data-table'
import { Skeleton } from '$lib/components/ui/skeleton'

const overviewQuery = createQuery(() => ({
  queryKey: ['stats-overview'],
  queryFn: () => admin.stats.overview(),
  refetchInterval: 10_000,
}))
const hourlyQuery = createQuery(() => ({
  queryKey: ['stats-hourly'],
  queryFn: () => admin.stats.hourly(24),
  refetchInterval: 30_000,
}))
const modelStatsQuery = createQuery(() => ({
  queryKey: ['stats-models'],
  queryFn: () => admin.stats.models(),
  refetchInterval: 30_000,
}))
const providerStatsQuery = createQuery(() => ({
  queryKey: ['stats-providers'],
  queryFn: () => admin.stats.providers(),
  refetchInterval: 30_000,
}))
const statusQuery = createQuery(() => ({ queryKey: ['gateway-status'], queryFn: admin.settings.status }))
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))

const overview = $derived(overviewQuery.data)
const modelStats = $derived(modelStatsQuery.data ?? [])
const providerStats = $derived(providerStatsQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const modelStatsColumnHelper = createDataTableColumnHelper<ModelStats>()
const modelStatsColumns = modelStatsColumnHelper.columns([
  modelStatsColumnHelper.accessor('model', {
    header: () => m.common_model(),
    meta: { label: () => m.common_model(), cellClass: 'font-technical font-medium' },
  }),
  modelStatsColumnHelper.accessor('request_count', {
    header: () => m.common_request_count_label(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.common_request_count_label(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  modelStatsColumnHelper.accessor((model) => model.total_input_tokens + model.total_output_tokens, {
    id: 'tokens',
    header: () => m.common_token(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.common_token(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  modelStatsColumnHelper.accessor('avg_duration_ms', {
    header: () => m.common_latency(),
    cell: (context) => formatDuration(context.getValue()),
    meta: { label: () => m.common_latency(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
])
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
  providerStatsColumnHelper.accessor('avg_duration_ms', {
    header: () => m.common_latency(),
    cell: (context) => formatDuration(context.getValue()),
    meta: { label: () => m.common_latency(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
])
const enabledModelCount = $derived(modelsQuery.data?.filter((model) => model.is_enabled).length)
const hasTraffic = $derived((overview?.total_requests ?? 0) > 0)
const requestChart = $derived(
  (hourlyQuery.data ?? []).map((item) => ({
    hour: formatTime(item.hour),
    requests: item.request_count,
    errors: item.error_count,
  })),
)
const latencyChart = $derived(
  (hourlyQuery.data ?? []).map((item) => ({
    hour: formatTime(item.hour),
    firstToken: item.avg_first_token_ms == null ? null : item.avg_first_token_ms / 1000,
    duration: item.avg_duration_ms / 1000,
  })),
)
const errorRate = $derived(hasTraffic && overview ? (overview.error_count / overview.total_requests) * 100 : 0)
const dash = '–'
const metrics = $derived([
  { label: m.common_total_requests(), value: hasTraffic ? formatCompactCount(overview?.total_requests ?? 0) : dash },
  {
    label: m.overview_total_tokens(),
    value: hasTraffic
      ? formatCompactCount((overview?.total_input_tokens ?? 0) + (overview?.total_output_tokens ?? 0))
      : dash,
  },
  { label: m.common_avg_latency(), value: hasTraffic ? formatDuration(overview?.avg_duration_ms ?? 0) : dash },
  {
    label: m.common_error_rate(),
    value: hasTraffic ? formatPercent(errorRate / 100) : dash,
    tone: hasTraffic && (overview?.error_count ?? 0) > 0 ? ('error' as const) : undefined,
  },
  { label: m.common_model_services(), value: formatNumber(providersQuery.data?.length ?? 0) },
  { label: m.common_models(), value: formatNumber(modelsQuery.data?.length ?? 0) },
])

function getModelStatsRowId(model: ModelStats): string {
  return model.model
}

function getProviderStatsRowId(provider: ProviderStats): string {
  return provider.provider
}
</script>

<svelte:head><title>{m.overview_overview()} · Stravia</title></svelte:head>

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={overviewQuery.isError ? m.overview_status_unavailable() : m.overview_gateway_live()}
    tone={overviewQuery.isError ? 'error' : 'healthy'} />
{/snippet}

{#snippet modelStatsEmpty()}
  <p class="py-6 text-center text-sm text-muted-foreground">{m.overview_no_model_traffic_yet()}</p>
{/snippet}

{#snippet providerStatsEmpty()}
  <p class="py-6 text-center text-sm text-muted-foreground">{m.overview_no_model_service_traffic_yet()}</p>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.overview_workspace()}
    title={m.overview_overview()}
    description={m.overview_see_whether_stravia_ready_how_much_used_which()}
    meta={liveMeta} />

  {#if isTauri}<DesktopPortNotice />{/if}

  <RouteSpine
    apiKeyCount={apiKeysQuery.data?.length}
    {enabledModelCount}
    providerCount={providersQuery.data?.length}
    currentPath="/" />

  {#if overviewQuery.isPending}
    <div class="route-metric-strip" aria-label={m.overview_loading_overview_metrics()}>
      {#each Array(6) as _, index (index)}
        <div class="route-metric-strip__item"><Skeleton class="h-4 w-24" /><Skeleton class="mt-2 h-7 w-20" /></div>
      {/each}
    </div>
    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <Skeleton class="h-80 min-[1280px]:col-span-7" />
      <Skeleton class="h-80 min-[1280px]:col-span-5" />
    </div>
  {:else if overviewQuery.isError}
    <section class="route-section" aria-labelledby="overview-error-title">
      <h2 id="overview-error-title" class="route-section-title">{m.overview_overview_unavailable()}</h2>
      <p class="route-section-description text-destructive">
        {localizeBackendErrorMessage(overviewQuery.error)}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void overviewQuery.refetch()}>{m.common_retry()}</Button>
    </section>
  {:else}
    <MetricStrip {metrics} label={m.common_usage_summary()} />

    {#if hasTraffic}
      <div class="grid gap-6 min-[1280px]:grid-cols-12">
        <section class="route-section min-[1280px]:col-span-7" aria-labelledby="request-volume-title">
          <div class="route-section-header">
            <div>
              <h2 id="request-volume-title" class="route-section-title">{m.overview_request_volume()}</h2>
              <p class="route-section-description">
                {m.overview_requests_errors_during_last_24_hours()}
              </p>
            </div>
            <span class="font-technical text-xs text-muted-foreground tabular-nums">24h</span>
          </div>
          {#if requestChart.length > 0}
            <div class="h-72 min-w-0" aria-label={m.overview_request_volume_chart()}>
              <BarChart
                data={requestChart}
                x={(item) => item.hour}
                series={[
                  { key: 'requests', label: m.common_requests_label(), color: 'var(--chart-1)' },
                  { key: 'errors', label: m.common_errors_label(), color: 'var(--chart-5)' },
                ]}
                seriesLayout="group"
                props={{ xAxis: { ticks: 4 } }} />
            </div>
          {:else}
            <div class="grid h-72 place-items-center border-y text-sm text-muted-foreground">
              {m.overview_no_request_traffic_has_recorded()}
            </div>
          {/if}
        </section>

        <section class="route-section min-[1280px]:col-span-5" aria-labelledby="latency-title">
          <div class="route-section-header">
            <div>
              <h2 id="latency-title" class="route-section-title">{m.common_latency()}</h2>
              <p class="route-section-description">
                {m.overview_average_first_token_end_end_latency_over_same_period()}
              </p>
            </div>
            <div class="flex shrink-0 flex-col items-end gap-2">
              <StatusIndicator
                compact
                label={statusQuery.data?.status === 'running'
                  ? m.common_stravia_running()
                  : m.overview_status_unavailable()}
                tone={statusQuery.data?.status === 'running' ? 'healthy' : 'neutral'} />
              <div class="font-technical grid grid-cols-[auto_auto] gap-x-2 text-xs tabular-nums">
                <span class="text-muted-foreground">{m.logs_first_token_short()}</span>
                <span>{formatDurationSeconds(overview?.avg_first_token_ms)}</span>
                <span class="text-muted-foreground">{m.logs_duration_short()}</span>
                <span>{formatDurationSeconds(overview?.avg_duration_ms)}</span>
              </div>
            </div>
          </div>
          {#if latencyChart.length > 0}
            <div class="h-72 min-w-0" aria-label={m.overview_latency_chart()}>
              <LineChart
                data={latencyChart}
                x={(item) => item.hour}
                series={[
                  { key: 'firstToken', label: m.stats_first_token_seconds(), color: 'var(--chart-2)' },
                  { key: 'duration', label: m.stats_duration_seconds(), color: 'var(--chart-1)' },
                ]}
                props={{ xAxis: { ticks: 4 } }} />
            </div>
          {:else}
            <div class="grid h-72 place-items-center border-y text-sm text-muted-foreground">
              {m.overview_latency_appears_first_request()}
            </div>
          {/if}
        </section>
      </div>
    {:else}
      <p class="border-y py-6 text-sm text-muted-foreground">{m.overview_send_first_request()}</p>
    {/if}

    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <section class="route-section min-[1280px]:col-span-7" aria-labelledby="model-ranking-title">
        <div class="route-section-header">
          <div>
            <h2 id="model-ranking-title" class="route-section-title">
              {m.overview_most_used_models()}
            </h2>
            <p class="route-section-description">
              {m.overview_client_model_names_most_requests()}
            </p>
          </div>
        </div>
        <div class="route-desktop-table">
          <DataTable
            data={modelStats.slice(0, 6)}
            columns={modelStatsColumns}
            labels={tableLabels}
            getRowId={getModelStatsRowId}
            ariaLabel={m.overview_most_used_models()}
            empty={modelStatsEmpty}
            stripedRows />
        </div>
        <div class="route-mobile-list">
          {#if modelStats.length === 0}
            <p class="border-y py-8 text-center text-sm text-muted-foreground">
              {m.overview_no_model_traffic_yet()}
            </p>
          {:else}
            {#each modelStats.slice(0, 6) as model (model.model)}
              <div class="route-mobile-row">
                <div class="min-w-0">
                  <p class="font-technical truncate font-medium">{model.model}</p>
                  <p class="mt-1 text-xs text-muted-foreground">
                    {formatDuration(model.avg_duration_ms)} · {formatCompactCount(
                      model.total_input_tokens + model.total_output_tokens,
                    )}
                    {m.common_token()}
                  </p>
                </div>
                <p class="font-technical tabular-nums">{formatCompactCount(model.request_count)}</p>
              </div>
            {/each}
          {/if}
        </div>
      </section>

      <section class="route-section min-[1280px]:col-span-5" aria-labelledby="provider-ranking-title">
        <div class="route-section-header">
          <div>
            <h2 id="provider-ranking-title" class="route-section-title">
              {m.overview_model_service_performance()}
            </h2>
            <p class="route-section-description">
              {m.overview_provider_metrics_summary()}
            </p>
          </div>
        </div>
        <div class="route-desktop-table">
          <DataTable
            data={providerStats.slice(0, 6)}
            columns={providerStatsColumns}
            labels={tableLabels}
            getRowId={getProviderStatsRowId}
            ariaLabel={m.overview_model_service_performance()}
            empty={providerStatsEmpty}
            stripedRows />
        </div>
        <div class="route-mobile-list">
          {#if providerStats.length === 0}
            <p class="border-y py-8 text-center text-sm text-muted-foreground">
              {m.overview_no_model_service_traffic_yet()}
            </p>
          {:else}
            {#each providerStats.slice(0, 6) as provider (provider.provider)}
              <div class="route-mobile-row">
                <div class="min-w-0">
                  <p class="truncate font-medium">{provider.provider}</p>
                  <p class="mt-1 text-xs text-muted-foreground">
                    {formatDuration(provider.avg_duration_ms)} ·
                    <span class="text-destructive">{provider.error_count} {m.common_errors()}</span>
                  </p>
                </div>
                <p class="font-technical tabular-nums">{formatCompactCount(provider.request_count)}</p>
              </div>
            {/each}
          {/if}
        </div>
      </section>
    </div>
  {/if}
</div>
