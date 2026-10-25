<script lang="ts">
import type { Snippet } from 'svelte'
import * as m from '#lib/paraglide/messages.js'
import { formatLogTime } from '#lib/format.js'
import {
  observationConversationBlocks,
  observationConversationMessages,
  type ObservationChatMessage,
} from '#lib/observation-conversation.js'
import { observationConversationActivities } from '#lib/observation-activities.js'
import ObservationActivity from '#lib/components/observation-activity.svelte'
import ConversationMessage from '#lib/components/conversation-message.svelte'
import type { InteractionDetail, LiveContentBlock } from '#lib/types.js'
import StreamingMarkdown from '#lib/components/streaming-markdown.svelte'
import ObservationLogViewport from '#lib/components/observation-log-viewport.svelte'

let {
  detail,
  liveBlocks = [],
  liveActive = true,
  liveContentEpoch = 0,
  olderLoading = false,
  onolder,
}: {
  detail: InteractionDetail
  liveBlocks?: LiveContentBlock[]
  liveActive?: boolean
  liveContentEpoch?: number
  olderLoading?: boolean
  onolder?: () => Promise<void>
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
// Refreshing an equal snapshot or mounting its offscreen DOM is not a new source revision.
const activityKey = $derived(
  JSON.stringify([detail.interaction.last_event_sequence, liveBlocks.map((block) => [block.block_id, block.revision])]),
)
</script>

<div class="conversation-shell">
  <div class="conversation-viewport">
    <ObservationLogViewport
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
                  <StreamingMarkdown text={message.text} active={false} snapshotKey={liveContentEpoch} virtualized />
                {:else}
                  {#each responseBlocks.get(message.id) ?? [] as block (block.id)}
                    {#if block.kind === 'text'}
                      <StreamingMarkdown
                        text={block.text}
                        active={block.live && liveActive}
                        snapshotKey={liveContentEpoch}
                        virtualized
                        allowImages />
                    {:else if block.kind === 'result'}
                      <ObservationActivity activity={block.activity} virtualized resultOnly />
                    {:else}
                      <ObservationActivity activity={block} {liveActive} {liveContentEpoch} virtualized />
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
.conversation-messages {
  display: flex;
  flex-direction: column;
  gap: 1.5rem;
  padding: 1rem;
  /* 检查器可被拖宽，正文仍保持阅读宽度。 */
  max-width: calc(var(--reading-max-width) + 2rem);
}
</style>
