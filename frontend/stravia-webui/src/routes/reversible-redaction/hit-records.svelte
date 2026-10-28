<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { onMount, untrack } from 'svelte'
import { resolve } from '$app/paths'
import { createVirtualizer } from '@tanstack/svelte-virtual'
import { admin } from '#lib/admin-client.js'
import {
  HIT_HIGHLIGHT_QUERY,
  HIT_HIGHLIGHT_VALUE,
  HIT_RECORDS_PAGE_SIZE,
  appendHitRecords,
  hitRecordsRefreshDelay,
  hitSourceKind,
  refreshHitRecords,
  uniqueHits,
} from '#lib/credential-hit-records.js'
import { formatLogTime, formatNumber } from '#lib/format.js'
import { observationStatusLabel } from '#lib/observation-labels.js'
import { observationStatusTone } from '#lib/observation-event-summary.js'

import type { CredentialDiscoverySummary, CredentialHit, CredentialHitKind } from '#lib/types.js'

import StatusIndicator from '#lib/components/status-indicator.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { badgeVariants } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import HistoryIcon from '@lucide/svelte/icons/history'

interface Props {
  /** 命中记录页签是否正在显示；隐藏时不自动刷新。 */
  active: boolean
  ruleNames: ReadonlyMap<string, string>
  rulesPending: boolean
  onOpenRule: (ruleId: string) => void
  onTest: () => void
}

let { active, ruleNames, rulesPending, onOpenRule, onTest }: Props = $props()

const ESTIMATED_ROW_HEIGHT = 84
// 距列表末尾还剩这么多行时就预取下一页，避免滚动到底才开始等待。
const LOAD_AHEAD_ROWS = 8

let kind = $state<CredentialHitKind>('all')
let items = $state.raw<CredentialDiscoverySummary[]>([])
let nextCursor = $state<string | null>(null)
let headLoaded = $state(false)
let loadingMore = $state(false)
let refreshing = $state(false)
// 'head' 是首次加载失败，'more' 是翻页失败；后台自动刷新失败不进入这里。
let failure = $state<'head' | 'more' | null>(null)
let lastFetchAt = $state(0)
let atTop = $state(true)
let pageVisible = $state(true)
let scroller = $state<HTMLDivElement | null>(null)
let scrollbarWidth = $state(0)
// 筛选变化后丢弃仍在途中的旧请求结果。
let generation = 0

const kindLabel = $derived(
  kind === 'created'
    ? m.credential_protection_hits_filter_created()
    : kind === 'reused'
      ? m.credential_protection_hits_filter_reused()
      : m.credential_protection_hits_filter_all(),
)
const autoRefresh = $derived(active && pageVisible && atTop && headLoaded && !refreshing && failure !== 'head')

const virtualizer = createVirtualizer<HTMLDivElement, HTMLDivElement>({
  count: 0,
  getScrollElement: () => null,
  estimateSize: () => ESTIMATED_ROW_HEIGHT,
  overscan: 6,
})

onMount(() => {
  pageVisible = document.visibilityState === 'visible'
  void reset()
})

// 虚拟化器不会自己发现滚动容器与行集合的变化；每次换成新的 getItemKey 才会让它丢弃按旧下标缓存的测量。
$effect(() => {
  const snapshot = items
  const element = scroller
  untrack(() =>
    $virtualizer.setOptions({
      count: snapshot.length,
      getScrollElement: () => element,
      getItemKey: (index) => snapshot[index]?.interaction_id ?? index,
    }),
  )
})

$effect(() => {
  const lastRow = $virtualizer.getVirtualItems().at(-1)
  if (lastRow && lastRow.index >= items.length - LOAD_AHEAD_ROWS) untrack(() => void loadMore())
})

$effect(() => {
  if (!autoRefresh) return
  const timer = setTimeout(() => void refresh(), hitRecordsRefreshDelay(lastFetchAt, Date.now()))
  return () => clearTimeout(timer)
})

// 表头在滚动区之外，右侧让出滚动条宽度才能与行对齐。
$effect(() => {
  const element = scroller
  if (!element) return
  const measureScrollbar = () => (scrollbarWidth = element.offsetWidth - element.clientWidth)
  const observer = new ResizeObserver(measureScrollbar)
  observer.observe(element)
  measureScrollbar()
  return () => observer.disconnect()
})

function fetchPage(hitKind: CredentialHitKind, cursor?: string) {
  return admin.credentialProtection.discoveries({ kind: hitKind, limit: HIT_RECORDS_PAGE_SIZE, cursor })
}

async function reset(): Promise<void> {
  const current = ++generation
  const hitKind = kind
  items = []
  nextCursor = null
  headLoaded = false
  loadingMore = false
  refreshing = false
  failure = null
  atTop = true
  if (scroller) scroller.scrollTop = 0
  lastFetchAt = Date.now()
  try {
    const page = await fetchPage(hitKind)
    if (current !== generation) return
    items = page.items
    nextCursor = page.next_cursor
    headLoaded = true
  } catch {
    if (current === generation) failure = 'head'
  }
}

async function loadMore(): Promise<void> {
  if (!headLoaded || !nextCursor || loadingMore || failure !== null) return
  const current = generation
  loadingMore = true
  try {
    const page = await fetchPage(kind, nextCursor)
    if (current !== generation) return
    items = appendHitRecords(items, page.items)
    nextCursor = page.next_cursor
  } catch {
    if (current === generation) failure = 'more'
  } finally {
    if (current === generation) loadingMore = false
  }
}

async function refresh(): Promise<void> {
  if (!headLoaded || refreshing) return
  const current = generation
  const previousFetchAt = lastFetchAt
  refreshing = true
  lastFetchAt = Date.now()
  try {
    const page = await fetchPage(kind)
    if (current !== generation) return
    // 请求期间用户已离开顶部时丢弃结果：在读历史的人不该看到列表被顶下去；回到顶部后按上一次成功刷新的时间补刷。
    if (!atTop) {
      lastFetchAt = previousFetchAt
      return
    }
    const refreshed = refreshHitRecords({ items, nextCursor }, page)
    items = refreshed.items
    nextCursor = refreshed.nextCursor
  } catch {
    // 后台刷新失败保持现有列表，下一个周期自然重试；只有用户主动触发的加载才报错。
  } finally {
    if (current === generation) refreshing = false
  }
}

function retry(): void {
  if (failure === 'head') {
    void reset()
    return
  }
  failure = null
  void loadMore()
}

function changeKind(value: string): void {
  kind = value as CredentialHitKind
  void reset()
}

function handleScroll(): void {
  atTop = (scroller?.scrollTop ?? 0) <= 1
}

// 行高随 chip 换行而变，交给虚拟化器实测；行内下标变化后需要重新登记。
function measureRow(node: HTMLDivElement, _index: number) {
  $virtualizer.measureElement(node)
  return {
    update() {
      $virtualizer.measureElement(node)
    },
  }
}

function sourceLabel(source: string): string {
  switch (source) {
    case 'user_message':
      return m.credential_protection_source_user_message()
    case 'history_user_message':
      return m.credential_protection_source_history_user_message()
    case 'system_or_history':
      return m.credential_protection_source_system_or_history()
    case 'tool_arguments':
      return m.credential_protection_source_tool_arguments()
    case 'tool_result':
      return m.credential_protection_source_tool_result()
    case 'other_text':
      return m.credential_protection_source_other_text()
    default:
      return m.credential_protection_source_unknown()
  }
}

function statusTone(status: string): 'healthy' | 'warning' | 'error' | 'neutral' {
  const tone = observationStatusTone(status)
  return tone === 'success' ? 'healthy' : tone
}

function requestLogHref(record: CredentialDiscoverySummary): string {
  return resolve(
    `logs?interaction=${encodeURIComponent(record.interaction_id)}&${HIT_HIGHLIGHT_QUERY}=${HIT_HIGHLIGHT_VALUE}`,
  )
}
</script>

<svelte:document onvisibilitychange={() => (pageVisible = document.visibilityState === 'visible')} />

{#snippet hitChips(hits: CredentialHit[])}
  <ul class="flex min-w-0 flex-wrap items-center gap-1.5">
    {#each uniqueHits(hits) as hit (`${hit.rule_id}:${hitSourceKind(hit)}`)}
      {@const name = ruleNames.get(hit.rule_id)}
      <li class="min-w-0 max-w-full">
        {#if name}
          <button
            type="button"
            title={hit.rule_id}
            class={badgeVariants({
              variant: 'secondary',
              class: 'h-auto max-w-full cursor-pointer gap-1 py-1 text-left whitespace-normal hover:bg-secondary/80',
            })}
            onclick={() => onOpenRule(hit.rule_id)}>
            <span class="font-medium">{name}</span>
            <span aria-hidden="true" class="text-muted-foreground">←</span>
            <span class="sr-only">{m.credential_protection_hits_found_in()}</span>
            <span class="text-muted-foreground">{sourceLabel(hitSourceKind(hit))}</span>
          </button>
        {:else if rulesPending}
          <Skeleton class="h-5 w-40" />
        {:else}
          <span
            title={hit.rule_id}
            class={badgeVariants({ variant: 'outline', class: 'h-auto max-w-full gap-1 py-1 whitespace-normal' })}>
            <span class="font-technical">{hit.rule_id}</span>
            <span aria-hidden="true" class="text-muted-foreground">←</span>
            <span class="sr-only">{m.credential_protection_hits_found_in()}</span>
            <span class="text-muted-foreground">{sourceLabel(hitSourceKind(hit))}</span>
          </span>
        {/if}
      </li>
    {/each}
  </ul>
{/snippet}

<div class="flex shrink-0 flex-wrap items-center gap-x-4 gap-y-2">
  <Select.Root type="single" value={kind} onValueChange={changeKind}>
    <Select.Trigger class="w-40" aria-label={m.credential_protection_hits_filter_label()}>{kindLabel}</Select.Trigger>
    <Select.Content>
      <Select.Group>
        <Select.Item value="all">{m.credential_protection_hits_filter_all()}</Select.Item>
        <Select.Item value="created">{m.credential_protection_hits_filter_created()}</Select.Item>
        <Select.Item value="reused">{m.credential_protection_hits_filter_reused()}</Select.Item>
      </Select.Group>
    </Select.Content>
  </Select.Root>
</div>

{#if failure === 'head'}
  <Alert.Root variant="destructive" class="shrink-0"
    ><Alert.Description>{m.credential_protection_discoveries_failed()}</Alert.Description></Alert.Root>
  <Button class="shrink-0 self-start" variant="outline" onclick={retry}>{m.common_retry()}</Button>
{:else if !headLoaded}
  <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
    ><Spinner />{m.credential_protection_discoveries_loading()}</span>
{:else if !items.length}
  <Empty.Root class="min-h-72"
    ><Empty.Header
      ><Empty.Media variant="icon"><HistoryIcon /></Empty.Media>
      <Empty.Title>{m.credential_protection_no_records()}</Empty.Title>
      <Empty.Description>{m.credential_protection_discoveries_empty()}</Empty.Description></Empty.Header>
    <Empty.Content
      ><Button variant="outline" onclick={onTest}
        >{m.credential_protection_test_tab()}<ArrowRightIcon data-icon="inline-end" /></Button
      ></Empty.Content>
  </Empty.Root>
{/if}

<div
  class={['hit-table', { 'hit-table--hidden': !items.length }]}
  role="table"
  aria-label={m.credential_protection_discoveries_title()}
  aria-rowcount={nextCursor ? -1 : items.length + 1}>
  <div class="hit-header hit-grid" role="row" style:padding-right="{scrollbarWidth}px">
    <div role="columnheader">{m.credential_protection_discoveries_time()}</div>
    <div role="columnheader">{m.credential_protection_discoveries_api_key()}</div>
    <div role="columnheader">{m.credential_protection_hits_created()}</div>
    <div role="columnheader">{m.credential_protection_hits_reused()}</div>
    <div role="columnheader">{m.credential_protection_discoveries_status()}</div>
    <div role="columnheader"><span class="sr-only">{m.common_actions()}</span></div>
  </div>
  <div bind:this={scroller} class="hit-scroller" data-slot="hit-records-viewport" onscroll={handleScroll}>
    <div class="relative w-full" role="rowgroup" style:height="{$virtualizer.getTotalSize()}px">
      {#each $virtualizer.getVirtualItems() as row (row.key)}
        {@const record = items[row.index]}
        {#if record}
          <div
            class="hit-row hit-grid"
            role="row"
            aria-rowindex={row.index + 2}
            data-index={row.index}
            style:transform="translateY({row.start}px)"
            use:measureRow={row.index}>
            <div role="cell" class="hit-time">{formatLogTime(record.discovered_at)}</div>
            <div role="cell" class="hit-key">
              {record.api_key_name ?? m.credential_protection_discoveries_unknown_key()}
            </div>
            <div role="cell" class={['hit-created', { 'hit-empty': !record.created.length }]}>
              {#if record.created.length}
                <p class="mb-1 text-xs text-muted-foreground">
                  {m.credential_protection_discoveries_count({ count: formatNumber(record.new_credential_count) })}
                </p>
                {@render hitChips(record.created)}
              {:else}
                <span class="text-muted-foreground" aria-hidden="true">—</span>
              {/if}
            </div>
            <div role="cell" class={['hit-reused', { 'hit-empty': !record.reused.length }]}>
              {#if record.reused.length}
                <p class="hit-reused-label mb-1 text-xs text-muted-foreground">
                  {m.credential_protection_hits_reused()}
                </p>
                {@render hitChips(record.reused)}
              {:else}
                <span class="text-muted-foreground" aria-hidden="true">—</span>
              {/if}
            </div>
            <div role="cell" class="hit-status">
              <div class="flex flex-col items-start gap-0.5">
                <StatusIndicator
                  compact
                  label={observationStatusLabel(record.status)}
                  tone={statusTone(record.status)} />
                {#if record.observation_gap}
                  <StatusIndicator compact tone="warning" label={m.observation_event_observation_gap()} />
                {/if}
              </div>
            </div>
            <div role="cell" class="hit-action">
              <Button
                variant="ghost"
                size="icon-sm"
                href={requestLogHref(record)}
                aria-label={m.credential_protection_discoveries_open()}
                title={m.credential_protection_discoveries_open()}>
                <ArrowRightIcon />
              </Button>
            </div>
          </div>
        {/if}
      {/each}
    </div>
    {#if loadingMore}
      <div class="hit-footer text-sm text-muted-foreground" role="status">
        <Spinner />{m.credential_protection_discoveries_loading()}
      </div>
    {:else if failure === 'more'}
      <div class="hit-footer">
        <Alert.Root variant="destructive"
          ><Alert.Description>{m.credential_protection_discoveries_failed()}</Alert.Description></Alert.Root>
        <Button variant="outline" size="sm" onclick={retry}>{m.common_retry()}</Button>
      </div>
    {/if}
  </div>
</div>

<style>
/* 记录较少时保持自然高度，较多时在滚动区内部滚动。 */
.hit-table {
  display: flex;
  flex: 0 1 auto;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  border: 1px solid color-mix(in oklab, var(--border) 60%, transparent);
  border-radius: var(--radius-md, 0.375rem);
}
.hit-table--hidden {
  display: none;
}
.hit-scroller {
  flex: 0 1 auto;
  min-height: 0;
  overflow-y: auto;
  overscroll-behavior: contain;
}
.hit-grid {
  display: grid;
  grid-template-columns: 9.5rem 8rem minmax(0, 1fr) minmax(0, 1fr) 7.5rem 2.5rem;
  column-gap: 1rem;
  align-items: start;
  padding-inline: 0.75rem;
}
.hit-header {
  flex-shrink: 0;
  align-items: center;
  padding-block: 0.5rem;
  border-bottom: 1px solid var(--border);
  background: color-mix(in oklab, var(--muted) 45%, var(--background));
  color: var(--muted-foreground);
  font-size: 0.75rem;
  font-weight: 500;
}
.hit-row {
  position: absolute;
  top: 0;
  left: 0;
  width: 100%;
  padding-block: 0.75rem;
  border-bottom: 1px solid color-mix(in oklab, var(--border) 60%, transparent);
  font-size: 0.875rem;
}
.hit-time {
  font-family: var(--font-technical);
  font-size: 0.75rem;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.hit-key {
  overflow-wrap: anywhere;
}
.hit-reused-label {
  display: none;
}
.hit-action {
  justify-self: end;
}
.hit-footer {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.75rem;
  padding: 0.75rem;
}
/* 窄屏改成堆叠行：无命中的一侧直接省略，复用组靠自己的小标题区分。 */
@container route-page (max-width: 52rem) {
  .hit-header {
    display: none;
  }
  .hit-grid {
    grid-template-columns: minmax(0, 1fr) auto;
    grid-template-areas:
      'key time'
      'created created'
      'reused reused'
      'status action';
    row-gap: 0.5rem;
  }
  .hit-key {
    grid-area: key;
    font-weight: 500;
  }
  .hit-time {
    grid-area: time;
  }
  .hit-created {
    grid-area: created;
  }
  .hit-reused {
    grid-area: reused;
  }
  .hit-status {
    grid-area: status;
  }
  .hit-action {
    grid-area: action;
  }
  .hit-empty {
    display: none;
  }
  .hit-reused-label {
    display: block;
  }
}
</style>
