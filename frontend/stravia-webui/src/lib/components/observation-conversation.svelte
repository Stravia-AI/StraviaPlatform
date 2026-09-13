<script lang="ts">
import { tick, untrack } from 'svelte'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
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
import { Button } from '$lib/components/ui/button'

let { detail, liveBlocks = [], olderLoading = false, onolder }: { detail: InteractionDetail; liveBlocks?: LiveContentBlock[]; olderLoading?: boolean; onolder?: () => Promise<void> } = $props()
let previousMessages: ObservationChatMessage[] = []
const messages = $derived.by(() => {
  previousMessages = observationConversationMessages(detail, liveBlocks, previousMessages)
  return previousMessages.filter((message) => message.role !== 'user' || message.text.trim())
})
let viewportElement: HTMLElement | undefined
let prepending = false
async function loadOlder(): Promise<void> {
  const viewport = viewportElement
  if (!viewport || !onolder || olderLoading) return
  inspectActivity?.()
  prepending = true
  const height = viewport.scrollHeight
  const top = viewport.scrollTop
  try {
    await onolder()
    await tick()
    viewport.scrollTop = top + viewport.scrollHeight - height
  } finally {
    prepending = false
  }
}
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
let hasNewActivity = $state(false)
let returnToLatest: (() => void) | undefined
let inspectActivity: (() => void) | undefined

function followConversation(viewport: HTMLElement): () => void {
  return untrack(() => {
    viewportElement = viewport
    const content = viewport.firstElementChild!
    let following = true
    let previousTop = viewport.scrollTop
    const atBottom = () => viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop <= 2
    const scrollToLatest = () => {
      viewport.scrollTop = viewport.scrollHeight
      previousTop = viewport.scrollTop
    }
    const resume = () => {
      following = true
      hasNewActivity = false
      scrollToLatest()
    }
    const onScroll = () => {
      const top = viewport.scrollTop
      if (top < previousTop || atBottom()) {
        following = atBottom()
        if (following) hasNewActivity = false
      }
      previousTop = top
    }
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) following = false
    }
    const onKey = (event: KeyboardEvent) => {
      if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) following = false
    }
    const resize = new ResizeObserver(() => {
      if (following) scrollToLatest()
    })
    const mutation = new MutationObserver((records) => {
      if (prepending) return
      // 用户展开已有详情并不是收到新内容；仍观察详情内部的实时文本变化。
      if (
        records.every(
          (record) =>
            record.type === 'childList' &&
            record.target instanceof Element &&
            record.target.getAttribute('data-slot') === 'collapsible',
        )
      )
        return
      if (following) scrollToLatest()
      else hasNewActivity = true
    })
    returnToLatest = resume
    inspectActivity = () => {
      following = false
    }
    scrollToLatest()
    resize.observe(content)
    resize.observe(viewport)
    mutation.observe(content, { childList: true, subtree: true, characterData: true })
    viewport.addEventListener('scroll', onScroll, { passive: true })
    viewport.addEventListener('wheel', onWheel, { passive: true })
    viewport.addEventListener('keydown', onKey)
    return () => {
      resize.disconnect()
      mutation.disconnect()
      viewport.removeEventListener('scroll', onScroll)
      viewport.removeEventListener('wheel', onWheel)
      viewport.removeEventListener('keydown', onKey)
      returnToLatest = undefined
      inspectActivity = undefined
    }
  })
}
</script>

<div class="conversation-shell">
  <!-- 独立滚动的消息记录需要参与 Tab 顺序，支持键盘翻阅。 -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="conversation-viewport"
    role="log"
    aria-label={m.observation_conversation()}
    aria-live="off"
    tabindex="0"
    {@attach followConversation}>
    <div class="conversation-messages">
      {#if detail.older_events_cursor !== null && onolder}
        <Button variant="outline" disabled={olderLoading} aria-busy={olderLoading} onclick={loadOlder}>{m.observation_load_earlier()}</Button>
      {/if}
      {#each groups as group (group[0].id)}
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
                    <ObservationActivity {activity} onInspect={() => inspectActivity?.()} />
                  {/if}
                {/each}
              {/if}
              <!-- 首次输出前保留流式实例，但不显示空白气泡。 -->
              <div class="bubble" hidden={!message.text}>
                <StreamingMarkdown text={message.text} active={!user && message.live} />
              </div>
              {#if !user}
                {#each activities.get(message.id) ?? [] as activity (activity.id)}
                  {#if activity.kind === 'tool'}
                    <ObservationActivity {activity} onInspect={() => inspectActivity?.()} />
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
      {/each}
    </div>
  </div>
  {#if hasNewActivity}
    <div class="latest-action">
      <Button variant="secondary" onclick={() => returnToLatest?.()}>
        <ArrowDownIcon data-icon="inline-start" />{m.observation_chat_latest()}
      </Button>
    </div>
  {/if}
</div>

<style>
.conversation-shell {
  position: relative;
  display: flex;
  height: 100%;
  min-height: 0;
  flex-direction: column;
}
.conversation-viewport {
  min-height: 0;
  flex: 1;
  overflow-y: auto;
  overscroll-behavior: contain;
  overflow-anchor: none;
  scrollbar-gutter: stable;
}
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
.latest-action {
  display: flex;
  justify-content: center;
  padding: 0.5rem 1rem max(0.5rem, env(safe-area-inset-bottom));
}
</style>
