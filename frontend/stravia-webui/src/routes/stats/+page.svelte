<script lang="ts">
import { onMount } from 'svelte'
import {
  liveStatsWindow,
  localStatsDateTime,
  parseLocalStatsDateTime,
  statsPresentationBucket,
} from '#lib/stats-window.js'
import type { StatsFilter, StatsWindow } from '#lib/types/stats.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { createQuery } from '@tanstack/svelte-query'
import type { RowSelectionState, SortingState } from '@tanstack/svelte-table'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import {
  formatCompactCount,
  formatDuration,
  formatList,
  formatLogTime,
  formatMinuteTime,
  formatMonthDay,
  formatPercent,
  formatTime,
  formatTps,
} from '#lib/format.js'
import {
  buildActivityGrid,
  buildAvailabilityChart,
  buildAvailabilitySummary,
  buildLatencyChart,
  hoverWindows,
  planActivityGrid,
  tokenBreakdown,
  type DonutSlice,
  type TokenCategoryKey,
} from '#lib/stats-chart.js'
import { formatMoney, hasMoneyRecords } from '#lib/money.js'
import { STATS_TOP_N, sortStatsRows, visibleStatsRows } from '#lib/stats-table.js'
import type { ApiKeyStats, ModelStats, ProviderStats, StatsOverview, StatsSeries } from '#lib/types.js'
import MetricStrip from '#lib/components/metric-strip.svelte'
import AvailabilityChart from '#lib/components/availability-chart.svelte'
import AvailabilitySummary from '#lib/components/availability-summary.svelte'
import LatencySpeedChart from '#lib/components/latency-speed-chart.svelte'
import LatencySpeedSummary from '#lib/components/latency-speed-summary.svelte'
import StatsDonut from '#lib/components/stats-donut.svelte'
import TokenActivityGrid from '#lib/components/token-activity-grid.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import { DataTable, createDataTableColumnHelper, type DataTableCell } from '#lib/components/ui/data-table/index.js'
import TimeWindowControls from '#lib/components/time-window-controls.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

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
// 每个问题就地显示在出问题的字段下；整刻钟规则本身只在对话框说明里出现一次。
const startIssue = $derived(draftStartMs == null ? m.stats_range_time_invalid() : null)
const endIssue = $derived.by(() => {
  if (draftEndMs == null) return m.stats_range_time_invalid()
  if (draftStartMs != null && draftEndMs <= draftStartMs) return m.stats_range_order_invalid()
  if (draftEndMs > liveStatsWindow(spanMs).end_at) return m.stats_range_future_invalid()
  return null
})
const validDraftRange = $derived(startIssue == null && endIssue == null)
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

// 供应商与 API 密钥互斥：长期汇总桶只按单一维度累计，二者无法组合筛选。
// 选择以行 id 记录；供应商只有 provider_id 非空的行可选。
let providerSelection = $state.raw<RowSelectionState>({})
let apiKeySelection = $state.raw<RowSelectionState>({})

// 两张明细表是选择来源，永远显示全部对象，不随筛选缩小。
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
const providerStats = $derived(providersQuery.data ?? [])
const apiKeyStats = $derived(apiKeysQuery.data ?? [])

// 已选 id 与当前窗口内的行取交集；行尚未加载时先按已选 id 请求，避免窗口切换时闪现未筛选的数据。
function selectedIds(selection: RowSelectionState, rowIds: readonly string[] | undefined): string[] {
  const rows = rowIds && new Set(rowIds)
  return Object.keys(selection)
    .filter((id) => selection[id] && (rows == null || rows.has(id)))
    .sort()
}
const statsFilter = $derived.by((): StatsFilter | undefined => {
  const providerIds = selectedIds(
    providerSelection,
    providersQuery.data?.flatMap((provider) => (provider.provider_id ? [provider.provider_id] : [])),
  )
  if (providerIds.length > 0) return { provider_ids: providerIds }
  const apiKeyIds = selectedIds(
    apiKeySelection,
    apiKeysQuery.data?.flatMap((apiKey) => (apiKey.api_key_id ? [apiKey.api_key_id] : [])),
  )
  return apiKeyIds.length > 0 ? { api_key_ids: apiKeyIds } : undefined
})
const filteredByProviders = $derived(statsFilter != null && 'provider_ids' in statsFilter)
const filtered = $derived(statsFilter != null)
// 占位数据正在被新结果替换时淡化面板；延迟 150ms 再淡化，本地快速返回的查询不会闪烁。
const BUSY = 'transition-opacity duration-[140ms] data-[busy=true]:opacity-60 data-[busy=true]:delay-150'
const selectedCount = $derived(
  statsFilter == null
    ? 0
    : 'provider_ids' in statsFilter
      ? statsFilter.provider_ids.length
      : statsFilter.api_key_ids.length,
)

// 查询键末尾固定附带所选窗口：悬停区间和筛选变化时，同一所选窗口内的旧数据可作占位继续显示并淡化；
// 所选窗口本身变化时不占位，避免把别的窗口的数据当成新窗口的结果。
function statsKey(name: string, view: StatsWindow, filter: StatsFilter | undefined, ...extra: unknown[]): unknown[] {
  return [name, view, ...extra, filter ?? null, window.start_at, window.end_at]
}
function keepWithinWindow<T>(
  previous: T | undefined,
  previousQuery: { queryKey: readonly unknown[] } | undefined,
): T | undefined {
  const key = previousQuery?.queryKey
  return key?.at(-2) === window.start_at && key.at(-1) === window.end_at ? previous : undefined
}

// Token 活动图固定跟随所选窗口，是悬停的来源，不随悬停区间移动。
const activityQuery = createQuery(() => {
  const plan = activityPlan
  const filter = statsFilter
  const range = { start_at: plan.startMs, end_at: plan.endMs }
  return {
    queryKey: statsKey('stats-series', range, filter, plan.bucketMs / 1_000),
    queryFn: () => admin.stats.series(range, plan.bucketMs / 1_000, filter),
    placeholderData: keepWithinWindow<StatsSeries[]>,
    enabled: activityWidth > 0 && activityHeight > 0,
    refetchInterval: 30_000,
  }
})
const activityGrid = $derived(
  buildActivityGrid(activityQuery.data ?? [], {
    startMs: activityPlan.startMs,
    endMs: activityPlan.endMs,
    bucketMs: activityQuery.data?.[0]?.bucket_ms ?? activityPlan.bucketMs,
  }),
)

// 悬停格：落稳后才驱动其余面板，避免指针划过时逐格发请求，也避免在方格间隙里来回闪烁。
const HOVER_ENTER_MS = 120
const HOVER_LEAVE_MS = 240
let hoverAt = $state.raw<{ start: number; windowStart: number; windowEnd: number } | null>(null)
let hoverTimer: ReturnType<typeof setTimeout> | undefined
function previewCell(start: number | null): void {
  clearTimeout(hoverTimer)
  hoverTimer = setTimeout(
    () => (hoverAt = start == null ? null : { start, windowStart: window.start_at, windowEnd: window.end_at }),
    start == null ? HOVER_LEAVE_MS : HOVER_ENTER_MS,
  )
}
// 方格随窗口重建后不会收到 pointerleave，所以悬停记录绑定落稳时的窗口，窗口变化即失效。
const hoverCell = $derived.by(() => {
  if (hoverAt == null || hoverAt.windowStart !== window.start_at || hoverAt.windowEnd !== window.end_at) return null
  const start = hoverAt.start
  return activityGrid.cells.find((cell) => cell.start === start) ?? null
})
const hover = $derived(hoverCell ? hoverWindows(window, hoverCell.start, activityGrid.bucketMs) : null)
// 延迟与成功率看与所选窗口等长、止于悬停格的区间；构成与模型只看悬停格自身的时段。
const viewWindow = $derived(hover?.view ?? window)
const periodWindow = $derived(hover?.period ?? window)
// 悬停时在构成与模型面板标题旁标明当前显示的时段，避免把悬停结果误读成所选窗口；
// 延迟与成功率的横轴本身就标出所显示的区间，不另加说明。
function formatPeriod({ start_at: start, end_at: end }: StatsWindow): string {
  const startDay = formatMonthDay(start)
  const endTime = formatMinuteTime(end)
  return `${startDay} ${formatMinuteTime(start)} – ${formatMonthDay(end) === startDay ? '' : `${formatMonthDay(end)} `}${endTime}`
}
const periodNote = $derived(hover ? formatPeriod(hover.period) : null)

const overviewQuery = createQuery(() => {
  const view = window
  const filter = statsFilter
  return {
    queryKey: statsKey('stats-overview', view, filter),
    queryFn: () => admin.stats.overview(view, filter),
    placeholderData: keepWithinWindow<StatsOverview>,
    refetchInterval: 30_000,
  }
})
// 未悬停时与 overviewQuery 同键，共用同一份缓存。
const viewOverviewQuery = createQuery(() => {
  const view = viewWindow
  const filter = statsFilter
  return {
    queryKey: statsKey('stats-overview', view, filter),
    queryFn: () => admin.stats.overview(view, filter),
    placeholderData: keepWithinWindow<StatsOverview>,
    refetchInterval: 30_000,
  }
})
const seriesQuery = createQuery(() => {
  const view = viewWindow
  const filter = statsFilter
  return {
    queryKey: statsKey('stats-series', view, filter, seriesBucketSeconds),
    queryFn: () => admin.stats.series(view, seriesBucketSeconds, filter),
    placeholderData: keepWithinWindow<StatsSeries[]>,
    refetchInterval: 30_000,
  }
})
// 模型占比只有全局口径，没有按供应商或 API 密钥的累计，筛选时不请求。
const modelsQuery = createQuery(() => {
  const view = periodWindow
  return {
    queryKey: ['stats-models', view, window.start_at, window.end_at],
    queryFn: () => admin.stats.models(view),
    placeholderData: keepWithinWindow<ModelStats[]>,
    enabled: statsFilter == null,
    refetchInterval: 30_000,
  }
})
onMount(() => {
  const timer = setInterval(() => {
    if (!live) return
    const current = liveStatsWindow(spanMs)
    if (current.end_at !== window.end_at) window = current
  }, 30_000)
  return () => {
    clearInterval(timer)
    clearTimeout(hoverTimer)
  }
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
// 延迟与成功率的汇总跟随悬停区间；其余汇总始终是所选窗口。
const viewOverview = $derived(viewOverviewQuery.data)
const hasTraffic = $derived((overview?.total_requests ?? 0) > 0)
const modelStats = $derived(modelsQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())

function clearSelection(): void {
  providerSelection = {}
  apiKeySelection = {}
}
// 手机列表没有 DataTable，选择动作在此复用；互斥规则与表格的 onSelectionChange 一致。
function setProviderSelected(id: string, selected: boolean): void {
  const next = { ...providerSelection }
  if (selected) next[id] = true
  else delete next[id]
  providerSelection = next
  if (selected) apiKeySelection = {}
}
function setApiKeySelected(id: string, selected: boolean): void {
  const next = { ...apiKeySelection }
  if (selected) next[id] = true
  else delete next[id]
  apiKeySelection = next
  if (selected) providerSelection = {}
}
const providerSelectable = (provider: ProviderStats): boolean =>
  provider.provider_id != null && provider.provider_id !== ''
// 空 id 表示未知或已删除的对象，后端无法按它筛选。
const apiKeySelectable = (apiKey: ApiKeyStats): boolean => apiKey.api_key_id !== ''

// 表格不渲染复选框列，选中态由行底色（加 aria-selected / aria-pressed）表达；
// `data-[state=selected]` 变体只命中被选中的行，所以对所有行返回同一个类即可。
const selectedRowClass = (): string => 'data-[state=selected]:bg-accent'
// 手机行向两侧外扩，让选中底色铺满内容区而不是贴着文字。
const MOBILE_SELECTABLE = '-mx-3 cursor-pointer px-3'
function activateOnKey(event: KeyboardEvent, action: () => void): void {
  if (event.target !== event.currentTarget || (event.key !== 'Enter' && event.key !== ' ')) return
  event.preventDefault()
  action()
}

function providerLabel(provider: ProviderStats): string {
  return `${provider.provider || provider.provider_id || '—'}${provider.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`
}
function apiKeyLabel(apiKey: ApiKeyStats): string {
  return `${apiKey.api_key_name || apiKey.api_key_id}${apiKey.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`
}

// 排序作用于完整集合，再取前 N 项；表格只展示结果，不自行排序。
let providerSorting = $state.raw<SortingState>([])
let apiKeySorting = $state.raw<SortingState>([])
let showAllProviders = $state(false)
let showAllApiKeys = $state(false)

// 上游错误率按该供应商自身的尝试计；没有上游请求时保持未知（“—”），不补成 0%。
function providerErrorRate(provider: ProviderStats): number | null {
  return provider.upstream_request_count > 0 ? provider.upstream_error_count / provider.upstream_request_count : null
}

function formatProviderErrorRate(provider: ProviderStats): string {
  const rate = providerErrorRate(provider)
  return rate == null ? '—' : formatPercent(rate)
}

const providerRows = $derived(
  sortStatsRows(providerStats, providerSorting, {
    provider: providerLabel,
    upstream_request_count: (provider) => provider.upstream_request_count,
    upstream_error_count: (provider) => provider.upstream_error_count,
    errorRate: providerErrorRate,
    avg_output_tps: (provider) => provider.avg_output_tps,
    allocated_cost: (provider) => (provider.allocated_cost ? Number(provider.allocated_cost.amount) : null),
  }),
)
const apiKeyRows = $derived(
  sortStatsRows(apiKeyStats, apiKeySorting, {
    apiKey: apiKeyLabel,
    request_count: (apiKey) => apiKey.request_count,
    total_input_tokens: (apiKey) => apiKey.total_input_tokens,
    total_output_tokens: (apiKey) => apiKey.total_output_tokens,
    cache_read_tokens: (apiKey) => apiKey.cache_read_tokens,
    cache_write_tokens: (apiKey) => apiKey.cache_write_tokens,
    last_used_at: (apiKey) => apiKey.last_used_at,
  }),
)
const visibleProviders = $derived(visibleStatsRows(providerRows, showAllProviders))
const visibleApiKeys = $derived(visibleStatsRows(apiKeyRows, showAllApiKeys))

// 零个上游错误保持中性色；只有真实错误才使用 destructive。
function providerCellClass(cell: DataTableCell<ProviderStats>): string | undefined {
  const provider = cell.row.original
  if (cell.column.id === 'upstream_error_count' && provider.upstream_error_count > 0) return 'text-destructive'
  return undefined
}
const providerStatsColumnHelper = createDataTableColumnHelper<ProviderStats>()
const providerStatsColumns = providerStatsColumnHelper.columns([
  providerStatsColumnHelper.accessor(providerLabel, {
    id: 'provider',
    header: () => m.common_model_service(),
    meta: { label: () => m.common_model_service(), headerClass: 'w-full', cellClass: 'font-medium' },
  }),
  providerStatsColumnHelper.accessor('upstream_request_count', {
    header: () => m.common_request_count_label(),
    cell: (context) => formatCompactCount(context.getValue()),
    meta: { label: () => m.common_request_count_label(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  providerStatsColumnHelper.accessor('upstream_error_count', {
    header: () => m.stats_error_count(),
    meta: { label: () => m.stats_error_count(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  providerStatsColumnHelper.accessor(formatProviderErrorRate, {
    id: 'errorRate',
    header: () => m.stats_error_rate(),
    cell: (context) => context.getValue(),
    meta: { label: () => m.stats_error_rate(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  providerStatsColumnHelper.accessor('avg_output_tps', {
    header: () => m.stats_output_speed(),
    cell: (context) => formatTps(context.getValue()),
    meta: { label: () => m.stats_output_speed(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
  providerStatsColumnHelper.accessor((provider) => formatMoney(provider.allocated_cost, provider.currency), {
    id: 'allocated_cost',
    header: () => m.cost_provider(),
    cell: (context) => context.getValue(),
    meta: { label: () => m.cost_provider(), align: 'end', cellClass: 'font-technical tabular-nums' },
  }),
])
const apiKeyStatsColumnHelper = createDataTableColumnHelper<ApiKeyStats>()
const apiKeyStatsColumns = apiKeyStatsColumnHelper.columns([
  apiKeyStatsColumnHelper.accessor(apiKeyLabel, {
    id: 'apiKey',
    header: () => m.common_api_key(),
    meta: { label: () => m.common_api_key(), headerClass: 'w-full', cellClass: 'font-medium' },
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
const seriesStats = $derived(seriesQuery.data ?? [])
const latencyChart = $derived(buildLatencyChart(seriesStats, seriesStats[0]?.bucket_ms ?? seriesBucketSeconds * 1_000))
const hasLatencyMetrics = $derived(latencyChart.some((point) => point.firstToken != null || point.outputTps != null))
// 窗口聚合取概览的总量相除，与折线同窗口但不经过桶边界；概览缺失时保持中性的未知值。
// 筛选后没有请求级的终结、失败与自动恢复分类，成功率不可得，不用别的口径代替。
const availabilityChart = $derived(
  filtered ? [] : buildAvailabilityChart(seriesStats, seriesStats[0]?.bucket_ms ?? seriesBucketSeconds * 1_000),
)
const availabilitySummary = $derived(
  !filtered && viewOverview ? buildAvailabilitySummary(viewOverview) : { final: null, firstPass: null },
)
const hasAvailability = $derived(availabilityChart.some((point) => point.final != null))

// 环形图按固定槽位对齐补间：同一分项在数据切换前后留在同一槽位，才能平滑变形而不是整圈重绘。
const MODEL_PALETTE = ['var(--chart-1)', 'var(--chart-2)', 'var(--chart-3)', 'var(--chart-4)', 'var(--muted)']
const MODEL_OTHER_SLOT = MODEL_PALETTE.length - 1
const modelSlices = $derived.by((): DonutSlice[] => {
  const slices = modelStats
    .slice(0, MODEL_OTHER_SLOT)
    .map((item, slot) => ({
      key: item.model_id ?? item.model,
      label: `${item.model || item.model_id || '—'}${item.deleted ? ` · ${m.stats_deleted_resource()}` : ''}`,
      value: item.request_count,
      slot,
    }))
  const rest = modelStats.slice(MODEL_OTHER_SLOT).reduce((total, item) => total + item.request_count, 0)
  if (rest > 0) slices.push({ key: '__other__', label: m.stats_other(), value: rest, slot: MODEL_OTHER_SLOT })
  return slices
})
const modelTotal = $derived(modelSlices.reduce((total, slice) => total + slice.value, 0))

const TOKEN_ORDER: readonly TokenCategoryKey[] = ['input', 'output', 'cache-read', 'cache-write']
const TOKEN_PALETTE = ['var(--chart-1)', 'var(--chart-2)', 'var(--chart-3)', 'var(--chart-4)']
// 图例只写分项名；“Token”是这块面板的共同单位，不在每一项后重复。
function tokenLabel(key: TokenCategoryKey): string {
  switch (key) {
    case 'input':
      return m.stats_input()
    case 'output':
      return m.stats_output()
    case 'cache-read':
      return m.stats_cache_read()
    case 'cache-write':
      return m.stats_cache_write()
  }
}
const EMPTY_TOKENS = {
  total_input_tokens: 0,
  total_output_tokens: 0,
  total_cache_read_tokens: 0,
  total_cache_write_tokens: 0,
}
// 悬停时的构成来自活动图同一时间桶的那一行，无需再请求；该时段没有记录就是零用量。
const tokenSource = $derived(
  hoverCell ? (activityQuery.data?.find((row) => row.bucket_start === hoverCell.start) ?? EMPTY_TOKENS) : overview,
)
// 未上报的类别不计入占比，避免把"未报告"显示成 0%。
const tokenPie = $derived.by((): { slices: DonutSlice[]; total: number } | null => {
  const source = tokenSource
  if (source == null) return null
  const slices = tokenBreakdown(source).map(({ key, value }) => ({
    key,
    label: tokenLabel(key),
    value,
    slot: TOKEN_ORDER.indexOf(key),
  }))
  return { slices, total: slices.reduce((sum, slice) => sum + slice.value, 0) }
})
// 没有任何成本记录（例如所有供应商都不计算成本）时显示中性的「—」，不把无数据画成 0。
function overviewCostValue(stats: StatsOverview | undefined): string {
  return hasMoneyRecords(stats?.actual_cost) ? formatMoney(stats.actual_cost, stats.currency) : '—'
}

const metrics = $derived([
  { label: m.common_total_requests(), value: formatCompactCount(overview?.total_requests) },
  { label: m.cost_total(), value: overviewCostValue(overview) },
  { label: m.stats_input_tokens(), value: formatCompactCount(overview?.total_input_tokens) },
  { label: m.stats_output_tokens(), value: formatCompactCount(overview?.total_output_tokens) },
  { label: m.stats_cache_read_tokens(), value: formatCompactCount(overview?.total_cache_read_tokens) },
  { label: m.stats_cache_write_tokens(), value: formatCompactCount(overview?.total_cache_write_tokens) },
  { label: m.common_avg_latency(), value: formatDuration(overview?.avg_duration_ms) },
])
const anyError = $derived(
  overviewQuery.error ??
    viewOverviewQuery.error ??
    seriesQuery.error ??
    activityQuery.error ??
    providersQuery.error ??
    apiKeysQuery.error ??
    modelsQuery.error,
)
// 先读取每个查询状态，避免短路跳过 TanStack 属性订阅后错过先完成的查询。
// 悬停区间与筛选变化时，旧数据作占位保留，只有首次加载或所选窗口变化才整页占位；
// 筛选时模型查询被停用，不能让它一直处于 pending。
const analyticsPending = $derived.by(() => {
  const overview = overviewQuery.isPending
  const series = seriesQuery.isPending
  const providers = providersQuery.isPending
  const apiKeys = apiKeysQuery.isPending
  const models = modelsQuery.isPending && !filtered
  return overview || series || providers || apiKeys || models
})
const analyticsFetching = $derived.by(() => {
  const overview = overviewQuery.isFetching
  const viewOverview = viewOverviewQuery.isFetching
  const series = seriesQuery.isFetching
  const activity = activityQuery.isFetching
  const providers = providersQuery.isFetching
  const apiKeys = apiKeysQuery.isFetching
  const models = modelsQuery.isFetching
  return overview || viewOverview || series || activity || providers || apiKeys || models
})
const failedAnalyticsLabels = $derived.by(() => {
  const labels: string[] = []
  if (overviewQuery.error || viewOverviewQuery.error) labels.push(m.stats_summary())
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
    viewOverviewQuery.refetch(),
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

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={analyticsFetching ? m.stats_updating_30s() : live ? m.stats_live_30s() : m.stats_fixed_window()}
    tone={anyError ? 'error' : 'healthy'} />
  {@render filterBanner()}
{/snippet}

{#snippet queryFailure(error: unknown, retry: () => unknown, retrying: boolean)}
  <RequestFailure message={localizeBackendErrorMessage(error)} {retry} {retrying} />
{/snippet}

{#snippet stateLine(text: string)}
  <p class="py-2 text-sm text-muted-foreground">{text}</p>
{/snippet}

<!-- 筛选状态放在标题行：它出现与消失不能改变页面高度，否则下方表格会在点击瞬间被推开，下一次点选落空。 -->
{#snippet filterBanner()}
  {#if filtered}
    <div class="flex items-center gap-x-2 text-sm" role="status">
      <span class="font-medium">
        {filteredByProviders
          ? m.stats_filtered_by_providers({ count: selectedCount })
          : m.stats_filtered_by_api_keys({ count: selectedCount })}
      </span>
      <Button type="button" variant="ghost" size="sm" onclick={clearSelection}>{m.stats_selection_clear()}</Button>
    </div>
  {/if}
{/snippet}

<!-- 悬停说明只在标题行内截断，绝不换行：它出现与消失不能改变标题行高度，否则指针下的方格会被挤到别处。 -->
{#snippet sectionTitle(id: string, title: string, hint?: string, note?: string | null)}
  <div class="flex min-w-0 items-center gap-x-2">
    <div class="flex shrink-0 items-center gap-1.5">
      <h2 {id} class="route-section-title">{title}</h2>
      {#if hint}<Field.Help {hint} />{/if}
    </div>
    {#if note}
      <span class="min-w-0 truncate font-technical text-xs text-muted-foreground tabular-nums" title={note}
        >{note}</span>
    {/if}
  </div>
{/snippet}

{#snippet topRange(total: number, showAll: boolean, toggle: () => void)}
  {#if total > STATS_TOP_N}
    <div class="flex items-center gap-3">
      {#if !showAll}
        <span class="font-technical text-xs text-muted-foreground tabular-nums"
          >{m.stats_top_range({ count: STATS_TOP_N, total })}</span>
      {/if}
      <Button type="button" variant="ghost" size="sm" onclick={toggle}
        >{showAll ? m.stats_show_top({ count: STATS_TOP_N }) : m.stats_view_all({ total })}</Button>
    </div>
  {/if}
{/snippet}

<div class="route-page">
  <PageHeader title={m.stats_usage_statistics()} meta={liveMeta} actions={rangeAction} />
  {#if analyticsPending}
    <MetricStrip loading loadingLabel={m.stats_loading_analytics_metrics()} placeholderCount={7} />
    <div class="grid gap-6 @min-[80rem]/route-page:grid-cols-12">
      <Skeleton class="h-52 @min-[80rem]/route-page:col-span-7" />
      <Skeleton class="h-52 @min-[80rem]/route-page:col-span-5" />
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
      <div data-busy={overviewQuery.isPlaceholderData} aria-busy={overviewQuery.isPlaceholderData} class={BUSY}>
        <MetricStrip {metrics} label={m.common_usage_summary()} />
      </div>
      {#if overview && !hasTraffic}
        <p class="text-sm text-muted-foreground">{m.stats_send_first_request()}</p>
      {/if}
    {/if}

    <!-- 12 列组合按工作区（route-page 容器）宽度切换，不看 viewport；活动与延迟共享标题行与 10rem 图面。 -->
    <div
      class="grid gap-8 @min-[80rem]/route-page:grid-cols-12 @min-[80rem]/route-page:gap-x-6 @min-[80rem]/route-page:gap-y-0">
      <section
        class="route-section flex flex-col @min-[80rem]/route-page:col-span-7 @min-[80rem]/route-page:row-span-2 @min-[80rem]/route-page:grid @min-[80rem]/route-page:grid-rows-subgrid"
        aria-labelledby="token-activity-title">
        <div class="route-section-header">
          {@render sectionTitle('token-activity-title', m.stats_token_activity(), m.stats_token_activity_help())}
        </div>
        <div
          class={['h-40 min-w-0 flex flex-col justify-center py-2', BUSY]}
          bind:clientWidth={activityWidth}
          bind:clientHeight={activityHeight}
          data-busy={activityQuery.isPlaceholderData}
          aria-busy={activityQuery.isPlaceholderData}
          data-activity-start={activityPlan.startMs}
          data-activity-end={activityPlan.endMs}
          data-activity-bucket={activityPlan.bucketMs}>
          {#if activityQuery.error && activityQuery.data === undefined}
            {@render queryFailure(activityQuery.error, activityQuery.refetch, activityQuery.isFetching)}
          {:else if activityWidth === 0 || activityHeight === 0 || activityQuery.isPending}
            <Skeleton class="h-full w-full" />
          {:else}
            <TokenActivityGrid model={activityGrid} cellPx={activityPlan.cellPx} onhover={previewCell} />
          {/if}
        </div>
      </section>

      <section
        class="route-section @min-[80rem]/route-page:col-span-5 @min-[80rem]/route-page:row-span-2 @min-[80rem]/route-page:grid @min-[80rem]/route-page:grid-rows-subgrid"
        aria-labelledby="latency-trend-title">
        <div class="route-section-header">
          {@render sectionTitle('latency-trend-title', m.stats_latency_speed())}
          <LatencySpeedSummary overview={viewOverview} />
        </div>
        <div
          class={['min-w-0', BUSY]}
          data-busy={seriesQuery.isPlaceholderData || viewOverviewQuery.isPlaceholderData}
          aria-busy={seriesQuery.isPlaceholderData || viewOverviewQuery.isPlaceholderData}>
          {#if seriesQuery.error && seriesQuery.data === undefined}
            {@render queryFailure(seriesQuery.error, seriesQuery.refetch, seriesQuery.isFetching)}
          {:else if seriesQuery.isPending}<Skeleton class="h-40" />
          {:else if hasLatencyMetrics}<div class="h-40 min-w-0">
              <LatencySpeedChart data={latencyChart} {formatBucket} />
            </div>{:else}{@render stateLine(m.stats_no_latency_speed_data())}{/if}
        </div>
      </section>
    </div>

    <!-- 占比分区：环形图与图例保持 10–12rem 的有界尺寸，宽度交给请求成功率趋势。 -->
    <div
      class="grid gap-8 @min-[36rem]/route-page:grid-cols-2 @min-[80rem]/route-page:grid-cols-[minmax(0,1fr)_minmax(15rem,22rem)_minmax(15rem,22rem)] @min-[80rem]/route-page:gap-x-6">
      <section
        class="route-section @min-[36rem]/route-page:col-span-2 @min-[80rem]/route-page:col-span-1"
        aria-labelledby="availability-title">
        <div class="route-section-header">
          {@render sectionTitle('availability-title', m.stats_availability())}
          <AvailabilitySummary summary={availabilitySummary} />
        </div>
        <div
          class={['min-w-0', BUSY]}
          data-busy={seriesQuery.isPlaceholderData || viewOverviewQuery.isPlaceholderData}
          aria-busy={seriesQuery.isPlaceholderData || viewOverviewQuery.isPlaceholderData}>
          {#if filtered}
            {@render stateLine(m.stats_filtered_availability_unavailable())}
          {:else if seriesQuery.error && seriesQuery.data === undefined}
            {@render queryFailure(seriesQuery.error, seriesQuery.refetch, seriesQuery.isFetching)}
          {:else if seriesQuery.isPending}<Skeleton class="h-40" />
          {:else if hasAvailability}<div class="h-40 min-w-0">
              <AvailabilityChart points={availabilityChart} {formatBucket} />
            </div>{:else}{@render stateLine(m.stats_no_availability_data())}{/if}
        </div>
      </section>

      <section class="route-section" aria-labelledby="token-breakdown-title">
        <div class="route-section-header">
          {@render sectionTitle(
            'token-breakdown-title',
            m.stats_token_breakdown(),
            m.stats_token_breakdown_share(),
            periodNote,
          )}
        </div>
        <div
          class={['min-h-40', BUSY]}
          data-busy={hoverCell ? activityQuery.isPlaceholderData : overviewQuery.isPlaceholderData}
          aria-busy={hoverCell ? activityQuery.isPlaceholderData : overviewQuery.isPlaceholderData}>
          {#if overviewQuery.error && overview === undefined}
            {@render queryFailure(overviewQuery.error, overviewQuery.refetch, overviewQuery.isFetching)}
          {:else if tokenPie && tokenPie.total > 0}
            <StatsDonut slices={tokenPie.slices} palette={TOKEN_PALETTE} ariaLabel={m.stats_token_breakdown()}>
              {#snippet center(active: DonutSlice | null)}
                <span class="font-technical text-xl font-medium tabular-nums"
                  >{formatCompactCount(active ? active.value : tokenPie.total)}</span>
              {/snippet}
            </StatsDonut>
          {:else}{@render stateLine(
              !hasTraffic
                ? m.stats_no_token_data()
                : tokenPie && tokenPie.slices.length > 0
                  ? m.stats_zero_tokens()
                  : m.stats_token_usage_unavailable(),
            )}{/if}
        </div>
      </section>

      <section class="route-section" aria-labelledby="analytics-model-title">
        <div class="route-section-header">
          {@render sectionTitle(
            'analytics-model-title',
            m.common_models(),
            m.stats_share_all_requests(),
            filtered ? null : periodNote,
          )}
        </div>
        <div
          class={['min-h-40', BUSY]}
          data-busy={modelsQuery.isPlaceholderData}
          aria-busy={modelsQuery.isPlaceholderData}>
          {#if filtered}
            {@render stateLine(m.stats_filtered_models_unavailable())}
          {:else if modelsQuery.error && modelsQuery.data === undefined}
            {@render queryFailure(modelsQuery.error, modelsQuery.refetch, modelsQuery.isFetching)}
          {:else if modelSlices.length > 0}
            <StatsDonut
              slices={modelSlices}
              palette={MODEL_PALETTE}
              ariaLabel={m.stats_share_all_requests()}
              labelClass="font-technical">
              {#snippet center(active: DonutSlice | null)}
                <span class="font-technical text-xl font-medium tabular-nums"
                  >{formatCompactCount(active ? active.value : modelTotal)}</span>
              {/snippet}
            </StatsDonut>
          {:else}{@render stateLine(m.stats_no_model_traffic())}{/if}
        </div>
      </section>
    </div>

    <!-- DataTable 根节点是 inline-size 容器，内容宽度无法向外传递，并排阈值只能按工作区宽度写死。
         两张表的数值列合计最宽约 530px / 860px（英文）；112rem 起仍给名称列各留出约 180px / 200px，
         API 密钥表列更多，因此分得更宽。 -->
    <div
      class="grid gap-8 @min-[112rem]/route-page:grid-cols-[minmax(0,2fr)_minmax(0,3fr)] @min-[112rem]/route-page:items-start @min-[112rem]/route-page:gap-x-6">
      <section class="route-section" aria-labelledby="analytics-provider-title">
        <div class="route-section-header">
          {@render sectionTitle('analytics-provider-title', m.common_model_services(), m.cost_provider_help())}
          {@render topRange(providerStats.length, showAllProviders, () => (showAllProviders = !showAllProviders))}
        </div>
        {#if providersQuery.error && providersQuery.data === undefined}
          {@render queryFailure(providersQuery.error, providersQuery.refetch, providersQuery.isFetching)}
        {:else if providerStats.length === 0}
          {@render stateLine(m.stats_no_model_service_traffic())}
        {:else}
          <div class="route-desktop-table">
            <DataTable
              data={visibleProviders}
              columns={providerStatsColumns}
              labels={tableLabels}
              getRowId={getProviderStatsRowId}
              ariaLabel={m.common_model_services()}
              cellClass={providerCellClass}
              manualSorting
              bind:sorting={providerSorting}
              selectionMode="multiple"
              showSelectionControl={false}
              metaKeySelection={false}
              rowSelectable={providerSelectable}
              rowClass={selectedRowClass}
              bind:rowSelection={providerSelection}
              onSelectionChange={(rows: ProviderStats[]) => {
                if (rows.length > 0) apiKeySelection = {}
              }}
              stripedRows />
          </div>
          <div class="route-mobile-list">
            {#each visibleProviders as provider (provider.provider_id ?? provider.provider)}{@const providerId =
                provider.provider_id ?? ''}{@const selectable = providerSelectable(provider)}{@const selected =
                selectable && Boolean(providerSelection[providerId])}
              <div
                class={['route-mobile-row', selectable && MOBILE_SELECTABLE, selected && 'bg-accent']}
                role="button"
                tabindex={selectable ? 0 : -1}
                aria-pressed={selected}
                aria-disabled={!selectable}
                onclick={() => selectable && setProviderSelected(providerId, !selected)}
                onkeydown={(event) =>
                  selectable && activateOnKey(event, () => setProviderSelected(providerId, !selected))}>
                <div class="min-w-0">
                  <p class="truncate font-medium">{providerLabel(provider)}</p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    {formatTps(provider.avg_output_tps)} ·
                    <span class={provider.upstream_error_count > 0 ? 'text-destructive' : undefined}
                      >{provider.upstream_error_count} {m.stats_errors()}</span>
                    · {formatProviderErrorRate(provider)}
                  </p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    {m.cost_provider()}: {formatMoney(provider.allocated_cost, provider.currency)}
                  </p>
                </div>
                <p class="font-technical tabular-nums">{formatCompactCount(provider.upstream_request_count)}</p>
              </div>{/each}
          </div>
        {/if}
      </section>

      <section class="route-section" aria-labelledby="analytics-key-title">
        <div class="route-section-header">
          <h2 id="analytics-key-title" class="route-section-title">{m.stats_api_key_usage()}</h2>
          {@render topRange(apiKeyStats.length, showAllApiKeys, () => (showAllApiKeys = !showAllApiKeys))}
        </div>
        {#if apiKeysQuery.error && apiKeysQuery.data === undefined}
          {@render queryFailure(apiKeysQuery.error, apiKeysQuery.refetch, apiKeysQuery.isFetching)}
        {:else if apiKeyStats.length === 0}
          {@render stateLine(m.stats_no_api_key_traffic())}
        {:else}
          <div class="route-desktop-table">
            <DataTable
              data={visibleApiKeys}
              columns={apiKeyStatsColumns}
              labels={tableLabels}
              getRowId={getApiKeyStatsRowId}
              ariaLabel={m.stats_api_key_usage()}
              manualSorting
              bind:sorting={apiKeySorting}
              selectionMode="multiple"
              showSelectionControl={false}
              metaKeySelection={false}
              rowSelectable={apiKeySelectable}
              rowClass={selectedRowClass}
              bind:rowSelection={apiKeySelection}
              onSelectionChange={(rows: ApiKeyStats[]) => {
                if (rows.length > 0) providerSelection = {}
              }}
              stripedRows />
          </div>
          <div class="route-mobile-list">
            {#each visibleApiKeys as apiKey (apiKey.api_key_id)}{@const selectable =
                apiKeySelectable(apiKey)}{@const selected = selectable && Boolean(apiKeySelection[apiKey.api_key_id])}
              <div
                class={['route-mobile-row', selectable && MOBILE_SELECTABLE, selected && 'bg-accent']}
                role="button"
                tabindex={selectable ? 0 : -1}
                aria-pressed={selected}
                aria-disabled={!selectable}
                onclick={() => selectable && setApiKeySelected(apiKey.api_key_id, !selected)}
                onkeydown={(event) =>
                  selectable && activateOnKey(event, () => setApiKeySelected(apiKey.api_key_id, !selected))}>
                <div class="min-w-0">
                  <p class="truncate font-medium">{apiKeyLabel(apiKey)}</p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    {m.observation_usage_input()}
                    {formatCompactCount(apiKey.total_input_tokens)} · {m.observation_usage_output()}
                    {formatCompactCount(apiKey.total_output_tokens)} ·
                    {m.observation_usage_cache_read()}
                    {formatCompactCount(apiKey.cache_read_tokens)} · {m.observation_usage_cache_write()}
                    {formatCompactCount(apiKey.cache_write_tokens)}
                  </p>
                  <p class="font-technical mt-1 text-xs text-muted-foreground">
                    {formatLogTime(apiKey.last_used_at)}
                  </p>
                </div>
                <p class="font-technical tabular-nums">{formatCompactCount(apiKey.request_count)}</p>
              </div>{/each}
          </div>
        {/if}
      </section>
    </div>
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
        <Field.Field data-invalid={startIssue != null || undefined}>
          <Field.FieldLabel for="stats-start">{m.stats_range_start()}</Field.FieldLabel>
          <Input
            id="stats-start"
            type="datetime-local"
            step="900"
            required
            bind:value={draftStart}
            aria-invalid={startIssue != null}
            aria-describedby={startIssue ? 'stats-start-error' : undefined} />
          {#if startIssue}<Field.FieldError id="stats-start-error">{startIssue}</Field.FieldError>{/if}
        </Field.Field>
        <Field.Field data-invalid={endIssue != null || undefined}>
          <Field.FieldLabel for="stats-end">{m.stats_range_end()}</Field.FieldLabel>
          <Input
            id="stats-end"
            type="datetime-local"
            step="900"
            required
            bind:value={draftEnd}
            aria-invalid={endIssue != null}
            aria-describedby={endIssue ? 'stats-end-error' : undefined} />
          {#if endIssue}<Field.FieldError id="stats-end-error">{endIssue}</Field.FieldError>{/if}
        </Field.Field>
      </Field.FieldGroup>
      <Dialog.Footer>
        <Button type="button" variant="outline" onclick={() => (rangeOpen = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={!validDraftRange}>{m.stats_apply_range()}</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>
