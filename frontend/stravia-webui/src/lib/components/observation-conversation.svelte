<script lang="ts">
import type { Snippet } from 'svelte'
import * as m from '$lib/paraglide/messages.js'
import { formatLogTime } from '$lib/format'
import { observationConversationMessages, type ObservationChatMessage } from '$lib/observation-conversation'
import { observationConversationActivities } from '$lib/observation-activities'
import ObservationActivity from '$lib/components/observation-activity.svelte'
import ConversationMessage from '$lib/components/conversation-message.svelte'
import type { InteractionDetail, LiveContentBlock } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import StreamingMarkdown from '$lib/components/streaming-markdown.svelte'
import ObservationLogViewport from '$lib/components/observation-log-viewport.svelte'

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
</script>

<ObservationLogViewport
  label={m.observation_conversation()}
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
        <ConversationMessage {user} {actor} label={actor}>
          {#each group as message (message.id)}
            {#if message.unsaved}<Badge variant="outline">{m.observation_live_unsaved()}</Badge>{/if}
            {#if !user}
              {#each activities.get(message.id) ?? [] as activity (activity.id)}
                {#if activity.kind === 'thinking'}
                  <ObservationActivity {activity} {liveActive} {liveContentEpoch} />
                {/if}
              {/each}
            {/if}
            <!-- 首次输出前保留流式实例，但不显示空白气泡。 -->
            <div hidden={!message.text}>
              <StreamingMarkdown
                text={message.text}
                active={!user && message.live && liveActive}
                snapshotKey={liveContentEpoch} />
            </div>
            {#if !user}
              {#each activities.get(message.id) ?? [] as activity (activity.id)}
                {#if activity.kind === 'tool'}
                  <ObservationActivity {activity} />
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

<style>
.conversation-messages {
  display: flex;
  flex-direction: column;
  gap: 1.5rem;
  padding: 1rem;
}
</style>
