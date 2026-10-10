<script lang="ts">
import type { Snippet } from 'svelte'
import BotIcon from '@lucide/svelte/icons/bot'
import UserIcon from '@lucide/svelte/icons/user'

let {
  user = false,
  label,
  actor,
  children,
  meta,
}: { user?: boolean; label: string; actor?: string; children: Snippet; meta?: Snippet } = $props()
</script>

<article class={['message', user && 'message-user']} aria-label={label}>
  {#if actor}
    <span class="actor-icon" aria-hidden="true">
      {#if user}<UserIcon size={16} />{:else}<BotIcon size={16} />{/if}
    </span>
  {/if}
  <div class="message-body">
    {#if actor}<h3 class="actor-name">{actor}</h3>{/if}
    <div class={['message-content', user && 'user-bubble']}>{@render children()}</div>
    {#if meta}<div class="message-meta">{@render meta()}</div>{/if}
  </div>
</article>

<style>
.message {
  display: flex;
  align-items: flex-start;
  gap: 0.5rem;
  width: 100%;
  min-width: 0;
}
.message-user {
  align-self: flex-end;
  flex-direction: row-reverse;
  max-width: 85%;
}
.actor-icon {
  display: grid;
  flex: none;
  place-items: center;
  width: 1.75rem;
  height: 1.75rem;
  border-radius: var(--radius-md);
  background: var(--muted);
  color: var(--muted-foreground);
}
.message-body {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
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
.message-content {
  display: flex;
  width: 100%;
  min-width: 0;
  flex-direction: column;
  gap: 0.75rem;
  font-size: 0.875rem;
  line-height: 1.58;
  overflow-wrap: anywhere;
}
.user-bubble {
  width: fit-content;
  max-width: 100%;
  border-radius: var(--radius-lg);
  background: var(--muted);
  padding: 0.75rem 1rem;
}
.message-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 0.5rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  font-variant-numeric: tabular-nums;
}
</style>
