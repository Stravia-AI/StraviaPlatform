<script lang="ts">
import type { Snippet } from 'svelte'
import BotIcon from '@lucide/svelte/icons/bot'
import UserIcon from '@lucide/svelte/icons/user'
import * as m from '$lib/paraglide/messages.js'
import { formatLogTime } from '$lib/format'
import { observationConversationMessages, type ObservationChatMessage } from '$lib/observation-conversation'
import { observationConversationActivities } from '$lib/observation-activities'
import ObservationActivity from '$lib/components/observation-activity.svelte'
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
        <article class={['message', user && 'message-user']} aria-label={actor}>
          <span class="actor-icon" aria-hidden="true">
            {#if user}<UserIcon size={16} />{:else}<BotIcon size={16} />{/if}
          </span>
          <div class="message-body">
            <h3 class="actor-name">{actor}</h3>
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
              <div class="bubble" hidden={!message.text}>
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
            <div class="message-meta">
              <time class="font-technical" datetime={new Date(group[group.length - 1].at).toISOString()}>
                {formatLogTime(group[group.length - 1].at)}
              </time>
            </div>
          </div>
        </article>
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
.message {
  display: flex;
  align-items: flex-start;
  gap: 0.5rem;
  width: 85%;
  min-width: 0;
}
.message-user {
  align-self: flex-end;
  flex-direction: row-reverse;
}
.actor-icon {
  display: grid;
  flex: none;
  place-items: center;
  width: 1.75rem;
  height: 1.75rem;
  border-radius: 0.5rem;
  background: var(--muted);
  color: var(--muted-foreground);
}
.message-user .actor-icon {
  color: var(--primary);
}
.message-body {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
  align-items: flex-start;
  gap: 0.375rem;
}
.message-user .message-body {
  align-items: flex-end;
}
.actor-name {
  max-width: 100%;
  padding-block: 0.25rem;
  overflow-wrap: anywhere;
  font-size: 0.75rem;
  font-weight: 500;
  color: var(--muted-foreground);
}
.bubble {
  max-width: 100%;
  min-width: 0;
  border-radius: 0.25rem 0.75rem 0.75rem;
  background: var(--muted);
  color: var(--foreground);
  padding: 0.75rem 1rem;
  font-size: 0.875rem;
  line-height: 1.58;
  overflow-wrap: anywhere;
}
.message-user .bubble {
  border-radius: 0.75rem 0.25rem 0.75rem 0.75rem;
  background: var(--primary);
  color: var(--primary-foreground);
}
.message-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 0.5rem;
  color: var(--muted-foreground);
  font-size: 0.6875rem;
  font-variant-numeric: tabular-nums;
}
.message-user .message-meta {
  justify-content: flex-end;
}
</style>
