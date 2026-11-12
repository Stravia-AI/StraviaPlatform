<script lang="ts">
import type { Snippet } from 'svelte'

let {
  user = false,
  label,
  children,
  meta,
  rows = false,
}: {
  user?: boolean
  label: string
  children: Snippet
  meta?: Snippet
  /** 虚拟列表把一条消息切成多行：`children` 直接是各行，气泡与间距由行自己绘制。 */
  rows?: boolean
} = $props()
</script>

<article class={['message', user && 'message-user']} aria-label={label}>
  {#if rows}
    <div class="message-rows">{@render children()}</div>
  {:else}
    <div class="message-body">
      <div class={['message-content', user && 'user-bubble']}>{@render children()}</div>
      {#if meta}<div class="message-meta">{@render meta()}</div>{/if}
    </div>
  {/if}
</article>

<style>
.message {
  display: flex;
  align-items: flex-start;
  width: 100%;
  min-width: 0;
}
.message-user {
  align-self: flex-end;
  max-width: 85%;
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
.message-rows {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
  font-size: 0.8125rem;
  line-height: 1.58;
  overflow-wrap: anywhere;
}
.message-content {
  display: flex;
  width: 100%;
  min-width: 0;
  flex-direction: column;
  gap: 0.75rem;
  font-size: 0.8125rem;
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
