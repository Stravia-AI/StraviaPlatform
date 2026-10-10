<script lang="ts">
import { untrack } from 'svelte'
import BrainIcon from '@lucide/svelte/icons/brain'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import WrenchIcon from '@lucide/svelte/icons/wrench'
import { toast } from 'svelte-sonner'
import * as m from '$lib/paraglide/messages.js'
import type { ObservationActivity } from '$lib/observation-activities'
import { cn } from '$lib/utils'
import StreamingMarkdown from '$lib/components/streaming-markdown.svelte'
import * as Collapsible from '$lib/components/ui/collapsible'
import * as Marker from '$lib/components/ui/marker'
import { Spinner } from '$lib/components/ui/spinner'

let {
  activity,
  liveActive = true,
  liveContentEpoch = 0,
  expansionPolicy = 'manual',
  minimumHeadingLevel = 1,
}: {
  activity: ObservationActivity
  liveActive?: boolean
  liveContentEpoch?: number
  expansionPolicy?: 'manual' | 'chat'
  minimumHeadingLevel?: 1 | 2
} = $props()
const contentId = $props.id()
const storageKey = $derived(`stravia:observation-activity:${activity.id}`)
const expandable = $derived(activity.kind === 'thinking' || activity.input !== undefined || activity.results.length > 0)
const label = $derived(
  activity.kind === 'thinking'
    ? activity.live
      ? m.observation_activity_thinking()
      : m.observation_activity_thought()
    : m.observation_activity_tool({ tool: activity.name }),
)
let open = $state(false)
let observedIdentity = ''
let observedPolicy: 'manual' | 'chat' | undefined
let wasLive = false
const toolReason = $derived.by(() => {
  if (activity.kind !== 'tool') return ''
  switch (activity.reason) {
    case 'superseded':
      return m.observation_activity_superseded()
    case 'client_disconnected':
      return m.observation_activity_client_disconnected()
    case 'client_wait_expired':
      return m.observation_activity_client_wait_expired()
    case 'process_restarted':
      return m.observation_activity_process_restarted()
    case 'user_interrupted':
    case 'interrupted':
    case 'cancelled':
      return m.observation_activity_interrupted()
    default:
      return ''
  }
})
const toolStatus = $derived.by(() => {
  if (activity.kind !== 'tool') return ''
  switch (activity.status) {
    case 'running':
      return m.observation_activity_running()
    case 'waiting':
      return m.observation_activity_waiting_client()
    case 'returned':
      return ''
    case 'error':
      return m.observation_activity_error()
    case 'missing-result':
      return toolReason || m.observation_activity_missing_result()
  }
})

function storageError(): void {
  toast.error(m.observation_activity_storage_error(), { id: 'observation-activity-storage' })
}

$effect(() => {
  const id = activity.id
  const live = activity.live
  const policy = expansionPolicy
  untrack(() => {
    if (observedIdentity !== id || observedPolicy !== policy) {
      observedIdentity = id
      observedPolicy = policy
      if (policy === 'chat') open = activity.kind === 'thinking' && live
      else {
        try {
          open = localStorage.getItem(storageKey) === 'true'
        } catch {
          open = false
          storageError()
        }
      }
    } else if (policy === 'chat' && wasLive && !live) {
      open = false
    }
    wasLive = live
  })
})

function changeOpen(next: boolean): void {
  open = next
  if (expansionPolicy === 'chat') return
  try {
    if (next) localStorage.setItem(storageKey, 'true')
    else localStorage.removeItem(storageKey)
  } catch {
    storageError()
  }
}

function formatContent(content: unknown): string {
  return typeof content === 'string' ? content : (JSON.stringify(content, null, 2) ?? '')
}
</script>

{#snippet markerContents()}
  <Marker.Icon>
    {#if activity.live}<Spinner />
    {:else if activity.kind === 'thinking'}<BrainIcon />
    {:else}<WrenchIcon />{/if}
  </Marker.Icon>
  <Marker.Content class="flex-1 [overflow-wrap:anywhere]">
    {label}
    {#if activity.kind === 'tool' && toolStatus}
      <span class="activity-state">{toolStatus}</span>
    {/if}
  </Marker.Content>
  {#if expandable}
    <ChevronRightIcon aria-hidden="true" class={open ? 'shrink-0 rotate-90' : 'shrink-0'} />
  {/if}
{/snippet}

<div class="activity" data-activity-id={activity.id} data-activity-kind={activity.kind} aria-busy={activity.live}>
  {#if expandable}
    <Collapsible.Root {open} onOpenChange={changeOpen} class="min-w-0 w-full">
      <div role={activity.live ? 'status' : undefined}>
        <Marker.Root>
          {#snippet child({ props }: { props: Record<string, unknown> })}
            <Collapsible.Trigger
              {...props}
              aria-controls={contentId}
              aria-label={label}
              class={cn(props.class as string, 'activity-trigger')}>
              {@render markerContents()}
            </Collapsible.Trigger>
          {/snippet}
        </Marker.Root>
      </div>
      <Collapsible.Content id={contentId} class="min-w-0">
        {#if open}
          <div class="activity-detail">
            {#if activity.kind === 'thinking'}
              <StreamingMarkdown
                text={activity.text}
                active={activity.live && liveActive && open}
                {minimumHeadingLevel}
                snapshotKey={liveContentEpoch} />
            {:else}
              {#if activity.input !== undefined}
                <div class="activity-section">
                  <h4>{m.observation_activity_tool_input()}</h4>
                  <pre class="font-technical">{formatContent(activity.input)}</pre>
                </div>
              {/if}
              {#each activity.results as result (result.id)}
                <div class="activity-section">
                  <h4>
                    {result.source === 'client'
                      ? m.observation_activity_client_result()
                      : m.observation_activity_platform_result()}
                    {#if result.isError}<span class="activity-error">{m.observation_activity_error()}</span>{/if}
                  </h4>
                  <pre class="font-technical">{formatContent(result.content)}</pre>
                </div>
              {/each}
            {/if}
          </div>
        {/if}
      </Collapsible.Content>
    </Collapsible.Root>
  {:else}
    <Marker.Root role={activity.live ? 'status' : undefined}>
      {@render markerContents()}
    </Marker.Root>
  {/if}
</div>

<style>
.activity {
  width: 100%;
  min-width: 0;
}
.activity :global(.activity-trigger) {
  display: flex;
  width: 100%;
  min-width: 0;
  min-height: 2.5rem;
  align-items: center;
  gap: 0.5rem;
  padding: 0.25rem;
  border-radius: 0.5rem;
  text-align: left;
  cursor: pointer;
}
.activity :global(.activity-trigger:hover) {
  background: var(--accent);
  color: var(--accent-foreground);
}
.activity-detail {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.75rem;
  padding: 0.5rem 0 0.75rem 0.75rem;
  border-left: 1px solid var(--border);
  font-size: 0.875rem;
  line-height: 1.58;
  overflow-wrap: anywhere;
}
.activity-section {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.375rem;
}
h4 {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 0.5rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  font-weight: 500;
}
pre {
  max-width: 100%;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.activity-error {
  color: var(--destructive);
}
.activity-state {
  margin-inline-start: 0.5rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
}
</style>
