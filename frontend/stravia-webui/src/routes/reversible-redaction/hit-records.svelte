<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { onMount, untrack } from 'svelte'
import { createVirtualizer } from '@tanstack/svelte-virtual'
import { admin } from '#lib/admin-client.js'
import {
  HIT_RECORDS_PAGE_SIZE,
  appendHitRecords,
  credentialSourceLabel,
  displayRuleSets,
  hitRecordsRefreshDelay,
  hitSourceKind,
  previewHits,
  refreshHitRecords,
  ruleSetLabel,
} from '#lib/credential-hit-records.js'
import { hitWindowQuery, type HitWindow } from '#lib/credential-hit-window.js'
import { formatLogTime, formatNumber } from '#lib/format.js'

import type { CredentialDiscoveryRecord, CredentialHit, CredentialRuleSet } from '#lib/types.js'

import HitRecordSheet from './hit-record-sheet.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import { Badge, badgeVariants } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import HistoryIcon from '@lucide/svelte/icons/history'

interface Props {
  /** 命中记录页签是否正在显示；隐藏时不自动刷新。 */
  active: boolean
  /** 按创建时间筛选的窗口；换成新对象后从第一页重新加载。 */
  range: HitWindow
  ruleNames: ReadonlyMap<string, string>
  rulesPending: boolean
  onOpenRule: (ruleId: string) => void
  onTest: () => void
}

let { active, range, ruleNames, rulesPending, onOpenRule, onTest }: Props = $props()

const ESTIMATED_ROW_HEIGHT = 92
// 距列表末尾还剩这么多行时就预取下一页，避免滚动到底才开始等待。
const LOAD_AHEAD_ROWS = 8

let items = $state.raw<CredentialDiscoveryRecord[]>([])
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
// 重置列表后丢弃仍在途中的旧请求结果。
let generation = 0

let detailOpen = $state(false)
let selectedRecord = $state.raw<CredentialDiscoveryRecord | null>(null)
let selectedHitIndex = $state(0)

const autoRefresh = $derived(
  active && range.live && pageVisible && atTop && headLoaded && !refreshing && failure !== 'head',
)

const virtualizer = createVirtualizer<HTMLDivElement, HTMLDivElement>({
  count: 0,
  getScrollElement: () => null,
  estimateSize: () => ESTIMATED_ROW_HEIGHT,
  overscan: 6,
})

onMount(() => {
  pageVisible = document.visibilityState === 'visible'
})

// 挂载与时间窗口变化都从第一页重新开始；页面只在用户操作时换新窗口对象。
$effect(() => {
  void range
  untrack(() => void reset())
})

// 虚拟化器不会自己发现滚动容器与行集合的变化；每次换成新的 getItemKey 才会让它丢弃按旧下标缓存的测量。
$effect(() => {
  const snapshot = items
  const element = scroller
  untrack(() =>
    $virtualizer.setOptions({
      count: snapshot.length,
      getScrollElement: () => element,
      getItemKey: (index) => snapshot[index]?.id ?? index,
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

function fetchPage(cursor?: string) {
  return admin.credentialProtection.discoveries({
    limit: HIT_RECORDS_PAGE_SIZE,
    cursor,
    ...hitWindowQuery(range, Date.now()),
  })
}

/** 由页面的刷新按钮调用：丢弃已加载的行，按当前窗口重新取第一页。 */
export function reload(): void {
  void reset()
}

async function reset(): Promise<void> {
  const current = ++generation
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
    const page = await fetchPage()
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
    const page = await fetchPage(nextCursor)
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
    const page = await fetchPage()
    if (current !== generation) return
    // 请求期间用户已离开顶部时丢弃结果：在读历史的人不该看到列表被顶下去；回到顶部后按上一次成功刷新的时间补刷。
    if (!atTop) {
      lastFetchAt = previousFetchAt
      return
    }
    const refreshed = refreshHitRecords({ items, nextCursor }, page, Date.now() - range.spanMs)
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

function handleScroll(): void {
  atTop = (scroller?.scrollTop ?? 0) <= 1
}

// 行高随标签换行而变，交给虚拟化器实测；行内下标变化后需要重新登记。
function measureRow(node: HTMLDivElement, _index: number) {
  $virtualizer.measureElement(node)
  return {
    update() {
      $virtualizer.measureElement(node)
    },
  }
}

function openRecord(record: CredentialDiscoveryRecord): void {
  selectedRecord = record
  selectedHitIndex = 0
  detailOpen = true
}

function handleRowKeydown(event: KeyboardEvent, record: CredentialDiscoveryRecord): void {
  // 行内还有规则按钮；只在行本身获得焦点时响应 Enter / Space。
  if (event.target !== event.currentTarget) return
  if (event.key !== 'Enter' && event.key !== ' ') return
  event.preventDefault()
  openRecord(record)
}

function openRuleFromChip(event: MouseEvent, ruleId: string): void {
  event.stopPropagation()
  onOpenRule(ruleId)
}
</script>

<svelte:document onvisibilitychange={() => (pageVisible = document.visibilityState === 'visible')} />

{#snippet ruleSetBadges(ruleSets: CredentialRuleSet[])}
  {#if ruleSets.length}
    <span class="flex flex-wrap items-center gap-1">
      {#each displayRuleSets(ruleSets) as ruleSet (ruleSet)}
        <Badge variant={ruleSet === 'builtin' ? 'outline' : 'secondary'}>{ruleSetLabel(ruleSet)}</Badge>
      {/each}
    </span>
  {:else}
    <span class="text-muted-foreground" aria-hidden="true">—</span>
  {/if}
{/snippet}

{#snippet createdRules(ruleIds: string[])}
  {#if ruleIds.length}
    <ul class="flex min-w-0 flex-wrap items-center gap-1.5">
      {#each ruleIds as ruleId (ruleId)}
        <li class="min-w-0 max-w-full">
          {#if ruleNames.get(ruleId)}
            <button
              type="button"
              title={ruleId}
              class={badgeVariants({
                variant: 'outline',
                class: 'h-auto max-w-full cursor-pointer py-1 text-left whitespace-normal hover:bg-accent',
              })}
              onclick={(event) => openRuleFromChip(event, ruleId)}>{ruleNames.get(ruleId)}</button>
          {:else if rulesPending}
            <Skeleton class="h-5 w-32" />
          {:else}
            <span
              title={ruleId}
              class={badgeVariants({ variant: 'outline', class: 'h-auto max-w-full py-1 font-technical' })}
              >{ruleId}</span>
          {/if}
        </li>
      {/each}
    </ul>
  {:else}
    <span class="text-muted-foreground" aria-hidden="true">—</span>
  {/if}
{/snippet}

{#snippet hitChips(hits: CredentialHit[], omitted: number)}
  {@const preview = previewHits(hits, omitted)}
  <ul class="flex min-w-0 flex-wrap items-center gap-1.5">
    {#each preview.hits as hit (`${hit.rule_id}:${hitSourceKind(hit)}`)}
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
            onclick={(event) => openRuleFromChip(event, hit.rule_id)}>
            <span class="font-medium">{name}</span>
            <span aria-hidden="true" class="text-muted-foreground">←</span>
            <span class="sr-only">{m.credential_protection_hits_found_in()}</span>
            <span class="text-muted-foreground">{credentialSourceLabel(hitSourceKind(hit))}</span>
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
            <span class="text-muted-foreground">{credentialSourceLabel(hitSourceKind(hit))}</span>
          </span>
        {/if}
      </li>
    {/each}
    {#if preview.hidden > 0}
      <li>
        <span class={badgeVariants({ variant: 'outline', class: 'py-1 tabular-nums' })}
          >{m.credential_protection_hit_records_more({ count: formatNumber(preview.hidden) })}</span>
      </li>
    {/if}
  </ul>
{/snippet}

<div class="hit-records">
  {#if failure === 'head'}
    <div class="hit-state">
      <RequestFailure class="w-full max-w-xl" message={m.credential_protection_discoveries_failed()} {retry} />
    </div>
  {:else if !headLoaded}
    <div class="hit-state">
      <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
        ><Spinner />{m.credential_protection_discoveries_loading()}</span>
    </div>
  {:else if !items.length}
    <div class="hit-state">
      <Empty.Root
        ><Empty.Header
          ><Empty.Media variant="icon"><HistoryIcon /></Empty.Media>
          <Empty.Title>{m.credential_protection_no_records_in_range()}</Empty.Title>
          <Empty.Description>{m.credential_protection_discoveries_empty_in_range()}</Empty.Description></Empty.Header>
        <Empty.Content
          ><Button variant="outline" onclick={onTest}
            >{m.credential_protection_test_tab()}<ArrowRightIcon data-icon="inline-end" /></Button
          ></Empty.Content>
      </Empty.Root>
    </div>
  {/if}

  <div
    class={['hit-table', { 'hit-table--hidden': !items.length }]}
    role="table"
    aria-label={m.credential_protection_discoveries_title()}
    aria-rowcount={nextCursor ? -1 : items.length + 1}>
    <div class="hit-header hit-grid" role="row" style:padding-right="{scrollbarWidth}px">
      <div role="columnheader">{m.credential_protection_hit_records_created_at()}</div>
      <div role="columnheader">{m.credential_protection_discoveries_api_key()}</div>
      <div role="columnheader">{m.credential_protection_hit_records_rule_sets()}</div>
      <div role="columnheader">{m.credential_protection_hit_records_created_by()}</div>
      <div role="columnheader">{m.credential_protection_hit_records_hits()}</div>
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
              tabindex="0"
              data-index={row.index}
              style:transform="translateY({row.start}px)"
              use:measureRow={row.index}
              onclick={() => openRecord(record)}
              onkeydown={(event) => handleRowKeydown(event, record)}>
              <div role="cell" class="hit-time">{formatLogTime(record.created_at)}</div>
              <div role="cell" class="hit-key">
                {record.api_key_name ?? m.credential_protection_discoveries_unknown_key()}
              </div>
              <div role="cell" class="hit-rulesets">
                <span class="hit-cell-label">{m.credential_protection_hit_records_rule_sets()}</span>
                {@render ruleSetBadges(record.rule_sets)}
              </div>
              <div role="cell" class="hit-rules">
                <span class="hit-cell-label">{m.credential_protection_hit_records_created_by()}</span>
                {@render createdRules(record.rule_ids)}
              </div>
              <div role="cell" class="hit-hits">
                <span class="hit-cell-label">{m.credential_protection_hit_records_hits()}</span>
                {#if record.hits.length}
                  {@render hitChips(record.hits, record.omitted_hits)}
                {:else}
                  <span class="text-muted-foreground" aria-hidden="true">—</span>
                {/if}
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
          <RequestFailure class="w-full" message={m.credential_protection_discoveries_failed()} {retry} />
        </div>
      {/if}
    </div>
  </div>
</div>

<HitRecordSheet bind:open={detailOpen} bind:hitIndex={selectedHitIndex} record={selectedRecord} {ruleNames} />

<style>
.hit-records {
  display: flex;
  flex: 1 1 auto;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
}
.hit-state {
  display: grid;
  flex: 1 1 auto;
  min-height: 0;
  place-items: center;
  padding: 2rem 1rem;
  overflow-y: auto;
}
/* 工作区外框已提供边界：表格贴边，记录较少时保持自然高度，较多时在滚动区内部滚动。 */
.hit-table {
  display: flex;
  flex: 0 1 auto;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
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
  grid-template-columns: 9.5rem 8rem 6.5rem minmax(0, 1fr) minmax(0, 1.3fr);
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
  cursor: pointer;
}
.hit-row:hover {
  background: var(--accent);
}
.hit-row:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: -2px;
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
.hit-cell-label {
  display: none;
}
.hit-footer {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.75rem;
  padding: 0.75rem;
}
/* 窄屏改成堆叠行：表头隐藏，每个字段靠自己的小标题区分。 */
@container route-page (max-width: 52rem) {
  .hit-header {
    display: none;
  }
  .hit-grid {
    grid-template-columns: minmax(0, 1fr) auto;
    grid-template-areas:
      'key time'
      'rulesets rulesets'
      'rules rules'
      'hits hits';
    row-gap: 0.5rem;
  }
  .hit-key {
    grid-area: key;
    font-weight: 500;
  }
  .hit-time {
    grid-area: time;
  }
  .hit-rulesets {
    grid-area: rulesets;
  }
  .hit-rules {
    grid-area: rules;
  }
  .hit-hits {
    grid-area: hits;
  }
  .hit-cell-label {
    display: block;
    margin-bottom: 0.25rem;
    color: var(--muted-foreground);
    font-size: 0.75rem;
  }
}
</style>
