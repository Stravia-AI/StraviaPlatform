<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createInfiniteQuery, createQuery } from '@tanstack/svelte-query'
import { resolve } from '$app/paths'
import type { CredentialDiscoveryRecord, CredentialHit } from '#lib/types.js'

import {
  HIT_REUSE_PAGE_SIZE,
  credentialSourceLabel,
  displayRuleSets,
  hitQueryFailure,
  hitsCoverRules,
  hitSourceKind,
  requestLogPath,
  reusePageBounds,
  ruleSetLabel,
} from '#lib/credential-hit-records.js'
import { credentialHitDetailQuery, credentialHitReusesQuery } from '#lib/credential-protection-queries.js'
import { formatLogTime, formatNumber } from '#lib/format.js'

import CredentialHitDetail from './credential-hit-detail.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Collapsible from '#lib/components/ui/collapsible/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'

interface Props {
  open?: boolean
  record: CredentialDiscoveryRecord | null
  hitIndex?: number
  ruleNames: ReadonlyMap<string, string>
}

let { open = $bindable(false), record, hitIndex = $bindable(0), ruleNames }: Props = $props()

let content = $state<HTMLElement | null>(null)
// 明文与复用分页都是侧栏内的临时视图状态：关闭侧栏、切换记录或命中时回到默认。
let revealMode = $state(false)
let reuseExpanded = $state(false)
let reuseRequested = $state(false)
let reusePage = $state(0)
let reuseScroller = $state<HTMLDivElement | null>(null)

const selectedHit = $derived(record?.hits[hitIndex] ?? null)
const maskedDetailQuery = createQuery(() =>
  credentialHitDetailQuery(record?.id ?? null, hitIndex, open && selectedHit !== null, false),
)
const revealedDetailQuery = createQuery(() =>
  credentialHitDetailQuery(record?.id ?? null, hitIndex, open && selectedHit !== null && revealMode, true),
)
const reusesQuery = createInfiniteQuery(() =>
  credentialHitReusesQuery(record?.id ?? null, open && record?.linkable === true && reuseRequested),
)

const revealedDetail = $derived(revealedDetailQuery.data)
const showingPlaintext = $derived(revealMode && revealedDetail?.state === 'available' && !revealedDetail.masked)
const displayedDetail = $derived(showingPlaintext && revealedDetail ? revealedDetail : maskedDetailQuery.data)
const revealPending = $derived(revealMode && revealedDetailQuery.isFetching && !showingPlaintext)
const revealFailure = $derived.by((): 'forbidden' | 'failed' | null => {
  if (!revealMode || !revealedDetailQuery.isError) return null
  return hitQueryFailure(revealedDetailQuery.error) === 'forbidden' ? 'forbidden' : 'failed'
})

const reusePages = $derived(reusesQuery.data?.pages ?? [])
const reuseTotal = $derived(reusePages[0]?.total ?? null)
const reuseLinkable = $derived(reusePages[0]?.linkable ?? true)
const reusePageData = $derived(reusePages[reusePage] ?? null)
const reuseItems = $derived(reusePageData?.items ?? [])
const reuseBounds = $derived(
  reusePageData && reuseTotal !== null
    ? reusePageBounds(reusePage, HIT_REUSE_PAGE_SIZE, reusePageData.items.length, reuseTotal)
    : null,
)
const reuseHasPrevious = $derived(reusePage > 0)
const reuseHasNext = $derived(reusePage < reusePages.length - 1 || reusesQuery.hasNextPage)

// 命中行已写出规则名时不再重复「创建规则」；没有命中定位或被省略时它是唯一的规则信息。
const showCreatedRules = $derived(record !== null && !hitsCoverRules(record.rule_ids, record.hits))

const title = $derived(
  record
    ? m.credential_protection_hit_records_detail_title({
        apiKey: record.api_key_name ?? m.credential_protection_discoveries_unknown_key(),
        time: formatLogTime(record.created_at),
      })
    : m.credential_protection_discoveries_title(),
)

function resetTransientState(): void {
  revealMode = false
  reuseExpanded = false
  reuseRequested = false
  reusePage = 0
}

function handleOpenChange(next: boolean): void {
  if (!next) resetTransientState()
}

// 关闭后重新打开同一条记录也要回到遮罩；记录本身切换时同样清空。
let activeRecordId = $state<string | null>(null)
$effect(() => {
  const id = record?.id ?? null
  if (id === activeRecordId) return
  activeRecordId = id
  resetTransientState()
})

function selectHit(index: number): void {
  hitIndex = index
  revealMode = false
}

function revealPlaintext(): void {
  revealMode = true
  // 权限不足或上次失败后再次点击是重试：查询已启用但停在错误态时手动重新请求。
  if (revealedDetailQuery.isError) void revealedDetailQuery.refetch()
}

function hidePlaintext(): void {
  revealMode = false
}

function setReuseExpanded(next: boolean): void {
  reuseExpanded = next
  if (next) reuseRequested = true
}

function showPreviousReusePage(): void {
  if (!reuseHasPrevious) return
  reusePage -= 1
  reuseScroller?.scrollTo({ top: 0 })
}

async function showNextReusePage(): Promise<void> {
  if (!reuseHasNext || reusesQuery.isFetchingNextPage) return
  if (reusePage + 1 >= reusePages.length) {
    const result = await reusesQuery.fetchNextPage()
    if (!result.data || result.data.pages.length <= reusePage + 1) return
  }
  reusePage += 1
  reuseScroller?.scrollTo({ top: 0 })
}

// 默认会聚焦第一个可聚焦元素，常常是工具 ID 的 Tooltip 触发器：脚本聚焦会弹出气泡，并让第一次 Escape 只关气泡。
function focusClose(event: Event): void {
  event.preventDefault()
  requestAnimationFrame(() => content?.querySelector<HTMLElement>('[data-slot="sheet-close"]')?.focus())
}
</script>

{#snippet hitLabel(hit: CredentialHit)}
  <span class="font-medium">{ruleNames.get(hit.rule_id) ?? hit.rule_id}</span>
  <span aria-hidden="true" class="text-muted-foreground">←</span>
  <span class="sr-only">{m.credential_protection_hits_found_in()}</span>
  <span class="text-muted-foreground">{credentialSourceLabel(hitSourceKind(hit))}</span>
{/snippet}

<Sheet.Root bind:open onOpenChange={handleOpenChange}>
  <Sheet.Content
    bind:ref={content}
    closeLabel={m.common_close()}
    onOpenAutoFocus={focusClose}
    class="data-[side=right]:w-full data-[side=right]:sm:max-w-xl overflow-y-auto">
    <Sheet.Header class="pr-12">
      <Sheet.Title>{title}</Sheet.Title>
    </Sheet.Header>
    {#if record}
      <div class="flex min-w-0 flex-col gap-6 px-4 pb-6">
        <dl class="summary-meta">
          <dt>{m.credential_protection_hit_records_rule_sets()}</dt>
          <dd>
            {#if record.rule_sets.length}
              <span class="flex flex-wrap gap-1.5">
                {#each displayRuleSets(record.rule_sets) as ruleSet (ruleSet)}
                  <Badge variant={ruleSet === 'builtin' ? 'outline' : 'secondary'}>{ruleSetLabel(ruleSet)}</Badge>
                {/each}
              </span>
            {:else}
              <span class="text-muted-foreground">—</span>
            {/if}
          </dd>
          {#if showCreatedRules}
            <dt>{m.credential_protection_hit_records_created_by()}</dt>
            <dd>
              <span class="flex flex-wrap gap-1.5">
                {#each record.rule_ids as ruleId (ruleId)}
                  <Badge variant="outline" class="h-auto max-w-full whitespace-normal" title={ruleId}
                    >{ruleNames.get(ruleId) ?? ruleId}</Badge>
                {/each}
              </span>
            </dd>
          {/if}
        </dl>

        <section class="flex min-w-0 flex-col gap-3" aria-labelledby="hit-list-title">
          <h3 id="hit-list-title" class="text-sm font-semibold">
            {m.credential_protection_hit_records_hits()}
          </h3>
          {#if record.hits.length === 0}
            <p class="text-sm text-muted-foreground">{m.credential_protection_hit_records_no_hits()}</p>
          {:else}
            {#if record.hits.length === 1}
              <!-- 只有一条命中时没有可选的东西，不画成按钮。 -->
              <p class="flex flex-wrap items-baseline gap-x-1.5 text-sm">{@render hitLabel(record.hits[0])}</p>
            {:else}
              <ul class="flex flex-col gap-1">
                {#each record.hits as hit, index (`${hit.rule_id}:${hitSourceKind(hit)}:${index}`)}
                  <li>
                    <Button
                      variant={index === hitIndex ? 'secondary' : 'ghost'}
                      class="h-auto w-full justify-start gap-1.5 whitespace-normal py-2 text-left"
                      aria-pressed={index === hitIndex}
                      onclick={() => selectHit(index)}>
                      {@render hitLabel(hit)}
                    </Button>
                  </li>
                {/each}
              </ul>
            {/if}

            {#if selectedHit}
              {#key `${record.id}:${hitIndex}`}
                {#if maskedDetailQuery.isPending}
                  <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
                    ><Spinner />{m.credential_protection_hit_records_detail_loading()}</span>
                {:else if maskedDetailQuery.isError}
                  {#if hitQueryFailure(maskedDetailQuery.error) === 'gone'}
                    <p class="text-sm text-muted-foreground">{m.credential_protection_hit_records_detail_gone()}</p>
                  {:else}
                    <Alert.Root variant="destructive"
                      ><Alert.Description>{m.credential_protection_hit_records_detail_failed()}</Alert.Description
                      ></Alert.Root>
                    <Button class="self-start" variant="outline" onclick={() => void maskedDetailQuery.refetch()}
                      >{m.common_retry()}</Button>
                  {/if}
                {:else if displayedDetail}
                  <CredentialHitDetail
                    detail={displayedDetail}
                    {ruleNames}
                    plaintext={showingPlaintext}
                    {revealPending}
                    {revealFailure}
                    onreveal={revealPlaintext}
                    onhide={hidePlaintext} />
                {/if}
              {/key}
            {/if}
          {/if}
        </section>

        <section class="flex min-w-0 flex-col gap-3" aria-labelledby="hit-reuse-title">
          {#if !record.linkable}
            <h3 id="hit-reuse-title" class="text-sm font-semibold">
              {m.credential_protection_hit_records_reuse()}
            </h3>
            <p class="text-sm text-muted-foreground">{m.credential_protection_hit_records_reuse_unlinkable()}</p>
          {:else}
            <Collapsible.Root open={reuseExpanded} onOpenChange={setReuseExpanded}>
              <h3 id="hit-reuse-title" class="text-sm font-semibold">
                <Collapsible.Trigger class="reuse-trigger group">
                  <span>{m.credential_protection_hit_records_reuse()}</span>
                  {#if reuseTotal !== null}
                    <span class="text-xs font-normal text-muted-foreground">{formatNumber(reuseTotal)}</span>
                  {/if}
                  <ChevronDownIcon
                    class="ms-auto size-4 shrink-0 text-muted-foreground transition-transform duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-data-[state=open]:rotate-180"
                    aria-hidden="true" />
                </Collapsible.Trigger>
              </h3>
              <Collapsible.Content class="min-w-0">
                {#if reuseExpanded}
                  {#if reusesQuery.isPending}
                    <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
                      ><Spinner />{m.credential_protection_hit_records_reuse_loading()}</span>
                  {:else if reusesQuery.isError && !reusePages.length}
                    {#if hitQueryFailure(reusesQuery.error) === 'gone'}
                      <p class="text-sm text-muted-foreground">{m.credential_protection_hit_records_detail_gone()}</p>
                    {:else}
                      <Alert.Root variant="destructive"
                        ><Alert.Description>{m.credential_protection_hit_records_reuse_failed()}</Alert.Description
                        ></Alert.Root>
                      <Button class="self-start" variant="outline" onclick={() => void reusesQuery.refetch()}
                        >{m.common_retry()}</Button>
                    {/if}
                  {:else if !reuseLinkable}
                    <p class="text-sm text-muted-foreground">
                      {m.credential_protection_hit_records_reuse_unlinkable()}
                    </p>
                  {:else if reuseTotal === 0}
                    <p class="text-sm text-muted-foreground">{m.credential_protection_hit_records_reuse_empty()}</p>
                  {:else if reuseTotal !== null}
                    <div class="reuse-box">
                      <div bind:this={reuseScroller} class="reuse-scroller" data-slot="reuse-viewport">
                        <ul class="reuse-list">
                          {#each reuseItems as item, index (`${index}:${item.interaction_id}:${item.last_reused_at}`)}
                            <li>
                              <a class="reuse-link" href={resolve(requestLogPath(item.interaction_id))}>
                                <span class="reuse-main">
                                  <span class="font-medium">{credentialSourceLabel(hitSourceKind(item))}</span>
                                  <span class="reuse-key"
                                    >{item.api_key_name ?? m.credential_protection_discoveries_unknown_key()}</span>
                                  <span class="reuse-time"
                                    >{m.credential_protection_hit_records_reuse_last({
                                      time: formatLogTime(item.last_reused_at),
                                    })}</span>
                                </span>
                                <span class="sr-only">{m.credential_protection_hit_records_open_request()}</span>
                                <ArrowRightIcon class="size-3.5 shrink-0 text-muted-foreground" aria-hidden="true" />
                              </a>
                            </li>
                          {/each}
                        </ul>
                      </div>
                      <div class="reuse-pager">
                        {#if reuseBounds}
                          <span class="font-technical text-xs text-muted-foreground"
                            >{m.credential_protection_hit_records_reuse_range({
                              start: formatNumber(reuseBounds.start),
                              end: formatNumber(reuseBounds.end),
                              total: formatNumber(reuseTotal),
                            })}</span>
                        {/if}
                        <span class="flex items-center gap-1">
                          <Button
                            size="icon-sm"
                            variant="ghost"
                            aria-label={m.common_previous()}
                            disabled={!reuseHasPrevious}
                            onclick={showPreviousReusePage}>
                            <ChevronLeftIcon />
                          </Button>
                          <Button
                            size="icon-sm"
                            variant="ghost"
                            aria-label={m.common_next()}
                            aria-busy={reusesQuery.isFetchingNextPage}
                            disabled={!reuseHasNext || reusesQuery.isFetchingNextPage}
                            onclick={() => void showNextReusePage()}>
                            {#if reusesQuery.isFetchingNextPage}
                              <Spinner class="size-3.5" />
                            {:else}
                              <ChevronRightIcon />
                            {/if}
                          </Button>
                        </span>
                      </div>
                    </div>
                    {#if reusesQuery.isError}
                      {#if hitQueryFailure(reusesQuery.error) === 'gone'}
                        <p class="text-sm text-muted-foreground">
                          {m.credential_protection_hit_records_detail_gone()}
                        </p>
                      {:else}
                        <Alert.Root variant="destructive"
                          ><Alert.Description>{m.credential_protection_hit_records_reuse_failed()}</Alert.Description
                          ></Alert.Root>
                        <Button class="self-start" variant="outline" onclick={() => void reusesQuery.refetch()}
                          >{m.common_retry()}</Button>
                      {/if}
                    {/if}
                  {/if}
                {/if}
              </Collapsible.Content>
            </Collapsible.Root>
          {/if}
        </section>
      </div>
    {/if}
  </Sheet.Content>
</Sheet.Root>

<style>
.summary-meta {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  align-items: baseline;
  gap: 0.5rem 1rem;
  font-size: 0.8125rem;
}
.summary-meta dt {
  color: var(--muted-foreground);
}
:global(.reuse-trigger) {
  display: flex;
  /* 负外边距让悬停底色外扩一圈，文字仍与下方的框左对齐；宽度要同步加回两侧外边距。 */
  width: calc(100% + 1rem);
  align-items: center;
  gap: 0.5rem;
  margin-inline: -0.5rem;
  padding: 0.25rem 0.5rem;
  border-radius: var(--radius-sm);
  text-align: start;
  transition: background-color 140ms cubic-bezier(0.2, 0, 0, 1);
}
:global(.reuse-trigger:hover) {
  background: var(--accent);
}
:global(.reuse-trigger:focus-visible) {
  outline: 2px solid var(--ring);
  outline-offset: 2px;
}
/* 与条目框一致的边框与底色；页脚分页留在滚动视口之外，始终可达。 */
.reuse-box {
  overflow: hidden;
  border: 1px solid var(--input);
  border-radius: var(--radius-lg);
  background: var(--background);
}
.reuse-scroller {
  /* 一页 10 行单行排布约 21rem；上限只兜底窄屏下换行变高的情形。 */
  max-height: 22rem;
  overflow: auto;
}
.reuse-list {
  display: flex;
  flex-direction: column;
}
.reuse-list > li + li {
  border-top: 1px solid var(--border);
}
.reuse-link {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem;
  padding: 0.3125rem 0.625rem;
  font-size: 0.8125rem;
  line-height: 1.25rem;
  transition: background-color 140ms cubic-bezier(0.2, 0, 0, 1);
}
.reuse-link:hover {
  background: var(--accent);
}
.reuse-link:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: -2px;
}
/* 来源、密钥与时间排成一行；放不下时时间折到下一行并靠右。 */
.reuse-main {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-wrap: wrap;
  align-items: baseline;
  column-gap: 0.625rem;
}
.reuse-key {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 0.75rem;
  color: var(--muted-foreground);
}
.reuse-time {
  margin-inline-start: auto;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  color: var(--muted-foreground);
}
.reuse-pager {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem;
  border-top: 1px solid var(--border);
  padding: 0.125rem 0.375rem 0.125rem 0.625rem;
}
</style>
