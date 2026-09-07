<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { BarChart, LineChart } from 'layerchart'

import { admin, isTauri } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { eligibleConnectKeys } from '$lib/connect'
import { getDataTableLabels } from '$lib/data-table-labels'
import {
  formatCompactCount,
  formatDuration,
  formatDurationSeconds,
  formatNumber,
  formatPercent,
  formatTime,
} from '$lib/format'
import { buildLatencyChart } from '$lib/stats-chart'
import type { ModelStats, ProviderStats } from '$lib/types'
import DesktopPortNotice from '$lib/components/desktop-port-notice.svelte'
import MetricStrip from '$lib/components/metric-strip.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RouteSpine from '$lib/components/route-spine.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import { Button } from '$lib/components/ui/button'
import * as Card from '$lib/components/ui/card'
import { DataTable, createDataTableColumnHelper } from '$lib/components/ui/data-table'
import * as Empty from '$lib/components/ui/empty'
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
const modelTokenTotal = (model: ModelStats): number | null =>
  model.total_input_tokens == null || model.total_output_tokens == null
    ? null
    : model.total_input_tokens + model.total_output_tokens
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
  modelStatsColumnHelper.accessor((model) => modelTokenTotal(model), {
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
const enabledProviders = $derived(providersQuery.data?.filter((provider) => provider.is_enabled))
const enabledProviderIds = $derived(new Set(enabledProviders?.map((provider) => provider.id)))
const connectableModels = $derived(
  modelsQuery.data?.filter(
    (model) =>
      model.is_enabled && model.targets.some((target) => target.enabled && enabledProviderIds.has(target.provider_id)),
  ),
)
const enabledModelCount = $derived(modelsQuery.data?.filter((model) => model.is_enabled).length)
const eligibleKeys = $derived(
  apiKeysQuery.data && connectableModels ? eligibleConnectKeys(apiKeysQuery.data, connectableModels) : undefined,
)
const configurationLoaded = $derived(
  providersQuery.data !== undefined && modelsQuery.data !== undefined && apiKeysQuery.data !== undefined,
)
const configurationError = $derived(providersQuery.error ?? modelsQuery.error ?? apiKeysQuery.error)
const setupAction = $derived.by(() => {
  const providers = providersQuery.data
  const models = modelsQuery.data
  const apiKeys = apiKeysQuery.data
  if (!providers || !models || !apiKeys || !enabledProviders || !connectableModels || !eligibleKeys) return undefined
  if (providers.length === 0) {
    return {
      title: m.overview_connect_service_title(),
      description: m.overview_connect_service_description(),
      label: m.common_connect_model_service(),
      href: '/providers' as const,
    }
  }
  if (enabledProviders.length === 0) {
    return {
      title: m.overview_enable_service_title(),
      description: m.overview_enable_service_description(),
      label: m.overview_review_model_services(),
      href: '/providers' as const,
    }
  }
  if (models.length === 0) {
    return {
      title: m.overview_add_model_title(),
      description: m.overview_add_model_description(),
      label: m.common_add_model(),
      href: '/models' as const,
    }
  }
  if (connectableModels.length === 0) {
    return {
      title: m.overview_review_models_title(),
      description: m.overview_review_models_description(),
      label: m.overview_review_models(),
      href: '/models' as const,
    }
  }
  if (apiKeys.length === 0) {
    return {
      title: m.overview_create_api_key_title(),
      description: m.overview_create_api_key_description(),
      label: m.common_create_api_key(),
      href: '/api-keys' as const,
    }
  }
  if (eligibleKeys.length === 0) {
    return {
      title: m.overview_review_api_keys_title(),
      description: m.overview_review_api_keys_description(),
      label: m.overview_review_api_keys(),
      href: '/api-keys' as const,
    }
  }
  return undefined
})
const hasTraffic = $derived((overview?.total_requests ?? 0) > 0)
const requestChart = $derived(
  (hourlyQuery.data ?? []).map((item) => ({
    hour: formatTime(item.hour),
    requests: item.request_count,
    errors: item.error_count,
  })),
)
const latencyChart = $derived(buildLatencyChart(hourlyQuery.data ?? [], formatTime))
const errorRate = $derived(hasTraffic && overview ? (overview.error_count / overview.total_requests) * 100 : 0)
const dash = '–'
const metrics = $derived([
  { label: m.common_total_requests(), value: hasTraffic ? formatCompactCount(overview?.total_requests ?? 0) : dash },
  {
    label: m.overview_total_tokens(),
    value:
      hasTraffic && overview?.total_input_tokens != null && overview.total_output_tokens != null
        ? formatCompactCount(overview.total_input_tokens + overview.total_output_tokens)
        : dash,
  },
  { label: m.common_avg_latency(), value: hasTraffic ? formatDuration(overview?.avg_duration_ms) : dash },
  {
    label: m.common_error_rate(),
    value: hasTraffic ? formatPercent(errorRate / 100) : dash,
    tone: hasTraffic && (overview?.error_count ?? 0) > 0 ? ('error' as const) : undefined,
  },
  {
    label: m.common_model_services(),
    value: providersQuery.data === undefined ? dash : formatNumber(providersQuery.data.length),
  },
  { label: m.common_models(), value: modelsQuery.data === undefined ? dash : formatNumber(modelsQuery.data.length) },
])

function getModelStatsRowId(model: ModelStats): string {
  return model.model
}

function getProviderStatsRowId(provider: ProviderStats): string {
  return provider.provider
}

function retryConfiguration(): void {
  void Promise.all([providersQuery.refetch(), modelsQuery.refetch(), apiKeysQuery.refetch()])
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

{#snippet connectAction()}
  <Button href="/connect">{m.connect_connect_apps()}</Button>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.overview_workspace()}
    title={m.overview_overview()}
    description={m.overview_see_whether_stravia_ready_how_much_used_which()}
    meta={liveMeta}
    actions={configurationLoaded && !setupAction ? connectAction : undefined} />

  {#if isTauri}<DesktopPortNotice />{/if}

  {#if configurationError}
    <section class="route-section" aria-labelledby="configuration-error-title">
      <h2 id="configuration-error-title" class="route-section-title">{m.overview_configuration_unavailable()}</h2>
      <p class="route-section-description text-destructive">
        {localizeBackendErrorMessage(configurationError)}
      </p>
      <Button class="mt-3" variant="outline" onclick={retryConfiguration}>{m.common_retry()}</Button>
    </section>
  {:else if !configurationLoaded}
    <Card.Root aria-label={m.overview_loading_configuration()}>
      <Card.Header>
        <Skeleton class="h-5 w-36" />
        <Skeleton class="h-4 w-full max-w-xl" />
      </Card.Header>
    </Card.Root>
  {/if}

  {#if configurationLoaded && setupAction}
    <Card.Root role="region" aria-labelledby="overview-next-action-title">
      <Card.Header>
        <Card.Title id="overview-next-action-title">{setupAction.title}</Card.Title>
        <Card.Description>{setupAction.description}</Card.Description>
        <Card.Action><Button href={setupAction.href}>{setupAction.label}</Button></Card.Action>
      </Card.Header>
    </Card.Root>
  {/if}

  <RouteSpine
    apiKeyCount={apiKeysQuery.data?.length}
    modelCount={modelsQuery.data?.length}
    {enabledModelCount}
    providerCount={providersQuery.data?.length}
    enabledProviderCount={enabledProviders?.length}
    currentPath="/" />

  {#if overviewQuery.isPending && overview === undefined}
    <div class="route-metric-strip" aria-label={m.overview_loading_overview_metrics()}>
      {#each Array(6) as _, index (index)}
        <div class="route-metric-strip__item"><Skeleton class="h-4 w-24" /><Skeleton class="mt-2 h-7 w-20" /></div>
      {/each}
    </div>
    <div class="grid gap-6 min-[1280px]:grid-cols-12">
      <Skeleton class="h-80 min-[1280px]:col-span-7" />
      <Skeleton class="h-80 min-[1280px]:col-span-5" />
    </div>
  {:else if overviewQuery.isError && overview === undefined}
    <section class="route-section" aria-labelledby="overview-error-title">
      <h2 id="overview-error-title" class="route-section-title">{m.overview_overview_unavailable()}</h2>
      <p class="route-section-description text-destructive">
        {localizeBackendErrorMessage(overviewQuery.error)}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void overviewQuery.refetch()}>{m.common_retry()}</Button>
    </section>
  {:else}
    {#if overviewQuery.isError}
      <section class="route-section" aria-labelledby="overview-refresh-error-title">
        <h2 id="overview-refresh-error-title" class="route-section-title">{m.overview_refresh_failed()}</h2>
        <p class="route-section-description text-destructive">
          {localizeBackendErrorMessage(overviewQuery.error)}
        </p>
        <Button class="mt-3" variant="outline" onclick={() => void overviewQuery.refetch()}>{m.common_retry()}</Button>
      </section>
    {/if}
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
                x={(item) => item.bucket}
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
      <Empty.Root class="border-y py-8">
        <Empty.Header>
          <Empty.Title>{m.overview_no_traffic_title()}</Empty.Title>
          <Empty.Description>{m.overview_send_first_request()}</Empty.Description>
        </Empty.Header>
      </Empty.Root>
    {/if}

    {#if hasTraffic}
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
                      {formatDuration(model.avg_duration_ms)} · {formatCompactCount(modelTokenTotal(model))}
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
  {/if}
</div>
