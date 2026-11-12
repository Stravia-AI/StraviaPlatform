<script lang="ts">
import { untrack, type Snippet } from 'svelte'
import * as m from '#lib/paraglide/messages.js'
import { formatLogTime } from '#lib/format.js'
import {
  observationConversationBlocks,
  observationConversationMessages,
  type ObservationChatMessage,
} from '#lib/observation-conversation.js'
import { observationConversationActivities } from '#lib/observation-activities.js'
import {
  estimateConversationRow,
  observationConversationRows,
  type ConversationRow,
  type ConversationSectionCache,
} from '#lib/observation-conversation-rows.js'
import type { VirtualRow } from '#lib/virtual-list.js'
import { resolveCredentialHits, type CredentialHitHighlight, type HitMark } from '#lib/credential-hit-highlights.js'
import CredentialHitMark from '#lib/components/credential-hit-mark.svelte'
import ObservationActivity from '#lib/components/observation-activity.svelte'
import ConversationMessage from '#lib/components/conversation-message.svelte'
import type { InteractionDetail, LiveContentBlock } from '#lib/types.js'
import MarkdownContent from '#lib/components/markdown-content.svelte'
import StreamingMarkdown from '#lib/components/streaming-markdown.svelte'
import VirtualLog from '#lib/components/virtual-log.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

let {
  detail,
  liveBlocks = [],
  liveActive = true,
  liveContentEpoch = 0,
  olderLoading = false,
  onolder,
  credentialHits,
  onjumprun,
}: {
  detail: InteractionDetail
  liveBlocks?: LiveContentBlock[]
  liveActive?: boolean
  liveContentEpoch?: number
  olderLoading?: boolean
  onolder?: () => Promise<void>
  /** 从命中记录进入时存在：高亮命中块并显示提示条。 */
  credentialHits?: CredentialHitHighlight
  /** 跳到诊断标签里的 Run；提示条据此给出解析不到块的命中的去处。 */
  onjumprun?: (runId: string) => void
} = $props()
let previousMessages: ObservationChatMessage[] = []
const messages = $derived.by(() => {
  previousMessages = observationConversationMessages(detail, liveBlocks, previousMessages)
  return previousMessages.filter((message) => message.role !== 'user' || message.text.trim())
})
const groups = $derived.by(() => {
  const result: (typeof messages)[] = []
  for (const message of messages) {
    const previousGroup = result.at(-1)
    const previous = previousGroup?.at(-1)
    if (
      previousGroup &&
      previous?.role === 'assistant' &&
      message.role === 'assistant' &&
      previous.model === message.model
    ) {
      previousGroup.push(message)
    } else {
      result.push([message])
    }
  }
  return result
})
const durableActivities = $derived(observationConversationActivities(detail))
const activities = $derived(observationConversationActivities(detail, liveBlocks, durableActivities))
const responseBlocks = $derived(observationConversationBlocks(detail, liveBlocks, activities))
const sectionCache: ConversationSectionCache = new Map()
const rows = $derived(observationConversationRows(groups, responseBlocks, sectionCache, liveActive))

const hits = $derived(
  credentialHits
    ? resolveCredentialHits(detail, {
        userMessageIdByRun: new Map(
          messages.flatMap((message) => (message.runId ? [[message.runId, message.id]] : [])),
        ),
        activities: [...durableActivities.values()].flat(),
      })
    : undefined,
)
const hitTotal = $derived(hits ? hits.created + hits.reused : 0)
const ruleNames = $derived(credentialHits?.ruleNames ?? new Map<string, string>())
// 命中可能落在尚未加载的更早事件里：先往前加载到出现新增命中；加载失败的游标不再重试，交给卷轴的重试按钮。
let stalledCursor = $state<number | null>(null)
const searching = $derived(
  credentialHits !== undefined &&
    (hits?.created ?? 0) === 0 &&
    detail.older_events_cursor !== null &&
    detail.older_events_cursor !== stalledCursor,
)
$effect(() => {
  if (!searching || olderLoading || !onolder) return
  const cursor = detail.older_events_cursor
  untrack(() => {
    void onolder().then(() => {
      if (detail.older_events_cursor === cursor) stalledCursor = cursor
    })
  })
})

/** 命中标记挂在哪一行：用户输入的首段、工具调用与结果所在的活动行。 */
function rowMarks(row: ConversationRow): (HitMark | undefined)[] {
  if (row.kind === 'section' && row.user && row.leading) return [hits?.userMessages.get(row.messageId)]
  if (row.kind === 'activity' && row.block.kind === 'tool')
    return [hits?.toolCalls.get(row.block.callId), hits?.toolResults.get(row.block.callId)]
  return []
}

let logViewport = $state<{ reveal(key: string): boolean }>()
let revealed = false
$effect(() => {
  void hits
  if (searching || revealed || !logViewport) return
  const marked = rows.filter((row) => rowMarks(row).some((mark) => mark !== undefined))
  const target = marked.find((row) => rowMarks(row).some((mark) => mark?.kind === 'created')) ?? marked[0]
  if (!target) return
  revealed = true
  untrack(() => logViewport?.reveal(target.key))
})
// Refreshing an equal snapshot or mounting its offscreen DOM is not a new source revision.
const activityKey = $derived(
  JSON.stringify([detail.interaction.last_event_sequence, liveBlocks.map((block) => [block.block_id, block.revision])]),
)
</script>

{#snippet conversationRow(row: ConversationRow)}
  <div class={['conversation-row', `space-${row.space}`, row.user && 'is-user', row.wide && 'is-wide']}>
    {#if row.kind === 'section'}
      <div class={['row-body', row.bubble && `bubble-${row.bubble}`]}>
        <CredentialHitMark marks={rowMarks(row)} {ruleNames}>
          <MarkdownContent text={row.section.source} deferred={row.section} allowImages={row.allowImages} />
        </CredentialHitMark>
      </div>
    {:else if row.kind === 'live'}
      <div class="row-body">
        <StreamingMarkdown text={row.block.text} active snapshotKey={liveContentEpoch} allowImages />
      </div>
    {:else if row.kind === 'activity'}
      {@const marks = rowMarks(row)}
      <div class="row-body">
        <CredentialHitMark {marks} {ruleNames}>
          <ObservationActivity
            activity={row.block}
            {liveActive}
            {liveContentEpoch}
            revealed={marks.some((mark) => mark !== undefined)} />
        </CredentialHitMark>
      </div>
    {:else}
      <time class="row-meta font-technical" datetime={new Date(row.at).toISOString()}>{formatLogTime(row.at)}</time>
    {/if}
  </div>
{/snippet}

{#snippet conversationSegment(
  _group: string,
  parts: VirtualRow<ConversationRow>[],
  measured: Snippet<[VirtualRow<ConversationRow>]>,
)}
  {@const first = parts[0].item}
  <ConversationMessage
    rows
    user={first.user}
    label={first.user ? m.observation_chat_you() : first.model || m.observation_chat_model()}>
    {#each parts as part (part.key)}{@render measured(part)}{/each}
  </ConversationMessage>
{/snippet}

<div class="conversation-shell">
  {#if credentialHits}
    <Alert.Root role="status" class="hit-banner">
      <div class="hit-banner-body">
        <p class="hit-banner-text">
          {#if searching}
            <Spinner class="inline size-3.5 align-text-bottom" />
            {m.observation_credential_hits_searching()}
          {:else if hitTotal > 0 && hits}
            {m.observation_credential_hits_summary({ total: hitTotal, created: hits.created, reused: hits.reused })}
            {#if hits.unresolved.length > 0}
              {m.observation_credential_hits_unresolved({ count: hits.unresolved.length })}
            {/if}
          {:else}
            {m.observation_credential_hits_none()}
          {/if}
        </p>
        <div class="hit-banner-actions">
          {#if onjumprun && hits && hits.unresolved.length > 0}
            <Button variant="outline" size="sm" onclick={() => onjumprun(hits.unresolved[0].runId)}>
              {m.observation_credential_hits_open_run()}
            </Button>
          {/if}
          <Button variant="outline" size="sm" onclick={credentialHits.onclear}>
            {m.observation_credential_hits_clear()}
          </Button>
        </div>
      </div>
    </Alert.Root>
  {/if}
  <div class="conversation-viewport">
    <VirtualLog
      bind:this={logViewport}
      label={m.observation_conversation()}
      {rows}
      rowKey={(row: ConversationRow) => row.key}
      estimateSize={estimateConversationRow}
      row={conversationRow}
      group={(row: ConversationRow) => row.group}
      segment={conversationSegment}
      {activityKey}
      olderCursor={detail.older_events_cursor}
      {olderLoading}
      {onolder}
      paddingStart={16}
      class="conversation-log" />
  </div>
</div>

<style>
.conversation-shell {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}
.conversation-viewport {
  flex: 1;
  min-height: 0;
}
.conversation-shell :global(.hit-banner) {
  flex: none;
  border-width: 0 0 1px;
  border-radius: 0;
}
.hit-banner-body {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem 0.75rem;
}
.hit-banner-text {
  min-width: 0;
  flex: 1 1 14rem;
  font-size: 0.8125rem;
  line-height: 1.5;
}
.hit-banner-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
}
.conversation-shell :global(.conversation-log) {
  /* 检查器可被拖宽，正文仍保持阅读宽度。 */
  max-width: calc(var(--reading-max-width) + 2rem);
  padding-inline: 1rem;
}
/* 行距由行自己的内边距承担：虚拟化器只量得到行高，量不到行与行之间的外边距。 */
.conversation-row.space-block {
  padding-bottom: 0.75rem;
}
.conversation-row.is-user .row-body {
  width: fit-content;
  max-width: 100%;
  margin-left: auto;
  background: var(--muted);
  padding-inline: 1rem;
}
.conversation-row.is-user.is-wide .row-body {
  width: 100%;
}
.conversation-row.is-user .row-body:is(.bubble-first, .bubble-only) {
  border-top-left-radius: var(--radius-lg);
  border-top-right-radius: var(--radius-lg);
  padding-top: 0.75rem;
}
.conversation-row.is-user .row-body:is(.bubble-last, .bubble-only) {
  border-bottom-left-radius: var(--radius-lg);
  border-bottom-right-radius: var(--radius-lg);
  padding-bottom: 0.75rem;
}
.row-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 0.5rem;
  padding-block: 0.375rem 1.5rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  font-variant-numeric: tabular-nums;
}
.is-user .row-meta {
  justify-content: flex-end;
}
</style>
