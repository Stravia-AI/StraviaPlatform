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
import { resolveCredentialHits, type CredentialHitHighlight } from '#lib/credential-hit-highlights.js'
import CredentialHitMark from '#lib/components/credential-hit-mark.svelte'
import ObservationActivity from '#lib/components/observation-activity.svelte'
import ConversationMessage from '#lib/components/conversation-message.svelte'
import type { InteractionDetail, LiveContentBlock } from '#lib/types.js'
import StreamingMarkdown from '#lib/components/streaming-markdown.svelte'
import ObservationLogViewport from '#lib/components/observation-log-viewport.svelte'
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

let shell = $state<HTMLElement>()
let logViewport = $state<{ reveal(element: HTMLElement): void }>()
let revealed = false
$effect(() => {
  void hits
  if (searching || revealed || !shell || !logViewport) return
  const target =
    shell.querySelector<HTMLElement>('[data-credential-hit="created"]') ??
    shell.querySelector<HTMLElement>('[data-credential-hit]')
  if (!target) return
  revealed = true
  logViewport.reveal(target)
})
// Refreshing an equal snapshot or mounting its offscreen DOM is not a new source revision.
const activityKey = $derived(
  JSON.stringify([detail.interaction.last_event_sequence, liveBlocks.map((block) => [block.block_id, block.revision])]),
)
</script>

<div class="conversation-shell" bind:this={shell}>
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
    <ObservationLogViewport
      bind:this={logViewport}
      label={m.observation_conversation()}
      {activityKey}
      olderCursor={detail.older_events_cursor}
      {olderLoading}
      {onolder}>
      {#snippet children(older: Snippet)}
        <div class="conversation-messages">
          <!-- 更早事件插在用户预览之后；没有用户预览时加载行落在卷轴顶部。 -->
          {#if groups[0]?.[0]?.role !== 'user'}{@render older()}{/if}
          {#each groups as group, index (group[0].id)}
            {@const user = group[0].role === 'user'}
            {@const actor = user ? m.observation_chat_you() : group[0].model || m.observation_chat_model()}
            <ConversationMessage {user} label={actor}>
              {#each group as message (message.id)}
                {#if user}
                  <CredentialHitMark marks={[hits?.userMessages.get(message.id)]} {ruleNames}>
                    <StreamingMarkdown text={message.text} active={false} snapshotKey={liveContentEpoch} virtualized />
                  </CredentialHitMark>
                {:else}
                  {#each responseBlocks.get(message.id) ?? [] as block (block.id)}
                    {#if block.kind === 'text'}
                      <StreamingMarkdown
                        text={block.text}
                        active={block.live && liveActive}
                        snapshotKey={liveContentEpoch}
                        virtualized
                        allowImages />
                    {:else}
                      {@const marks =
                        block.kind === 'tool'
                          ? [hits?.toolCalls.get(block.callId), hits?.toolResults.get(block.callId)]
                          : []}
                      <CredentialHitMark {marks} {ruleNames}>
                        <ObservationActivity
                          activity={block}
                          {liveActive}
                          {liveContentEpoch}
                          virtualized
                          revealed={marks.some((mark) => mark !== undefined)} />
                      </CredentialHitMark>
                    {/if}
                  {/each}
                {/if}
              {/each}
              {#snippet meta()}
                <time class="font-technical" datetime={new Date(group[group.length - 1].at).toISOString()}>
                  {formatLogTime(group[group.length - 1].at)}
                </time>
              {/snippet}
            </ConversationMessage>
            {#if index === 0 && user}{@render older()}{/if}
          {/each}
        </div>
      {/snippet}
    </ObservationLogViewport>
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
.conversation-messages {
  display: flex;
  flex-direction: column;
  gap: 1.5rem;
  padding: 1rem;
  /* 检查器可被拖宽，正文仍保持阅读宽度。 */
  max-width: calc(var(--reading-max-width) + 2rem);
}
</style>
