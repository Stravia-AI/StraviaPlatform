<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { renderSnippet } from '@tanstack/svelte-table'
import { getConsoleChat } from '#lib/console-chat.svelte.js'
import type { ConsoleConversation } from '#lib/console-chat-types.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatLogTime, formatNumber } from '#lib/format.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import * as m from '#lib/paraglide/messages.js'
import ConsoleChatActions from '#lib/components/console-chat-actions.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import SearchIcon from '@lucide/svelte/icons/search'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
} from '#lib/components/ui/data-table/index.js'
const chat = getConsoleChat()
let query = $state('')
const snapshot = $derived(chat.snapshot)
const conversations = $derived.by(() => {
  void snapshot.conversations
  return chat.findConversations(query)
})
const empty = $derived(!snapshot.loading && !snapshot.conversations.length && !snapshot.loadError)
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<ConsoleConversation>()
const columns = helper.columns([
  helper.accessor('title', {
    header: () => m.console_chat_title(),
    cell: (context) => renderSnippet(titleCell, context),
    meta: { label: () => m.console_chat_title(), width: 'fill' },
    size: 260,
  }),
  helper.accessor('apiKeyName', {
    header: () => m.console_chat_key(),
    meta: { label: () => m.console_chat_key(), width: 'compact' },
    size: 140,
  }),
  helper.accessor((item) => lastModel(item), {
    id: 'model',
    header: () => m.console_chat_model(),
    meta: { label: () => m.console_chat_model(), cellClass: 'font-technical text-xs', width: 'compact' },
    size: 160,
  }),
  helper.accessor((item) => item.messages.length, {
    id: 'messages',
    header: () => m.console_chat_messages(),
    meta: { label: () => m.console_chat_messages(), align: 'end', cellClass: 'font-technical', width: 'compact' },
    size: 90,
  }),
  helper.accessor('updatedAt', {
    header: () => m.console_chat_last_activity(),
    cell: (context) => formatLogTime(context.getValue()),
    meta: { label: () => m.console_chat_last_activity(), cellClass: 'font-technical text-xs', width: 'compact' },
    size: 160,
  }),
  helper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(actionsCell, context),
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end', width: 'compact' },
    size: 88,
  }),
])
function lastModel(item: ConsoleConversation) {
  const message = item.messages.findLast((candidate) => candidate.role === 'assistant')
  return message?.role === 'assistant' ? message.routeId : '—'
}
</script>

<svelte:head><title>{m.console_chat_all()} · Stravia</title></svelte:head>
{#snippet pageActions()}<Button href={resolve('/')}>{m.console_chat_new()}</Button><ConsoleChatActions
    clearAll />{/snippet}
{#snippet titleCell(context: DataTableCellContext<ConsoleConversation>)}
  {@const item = context.row.original}

  <a href={resolve(`?conversation=${encodeURIComponent(item.id)}`)} class="font-medium hover:underline">{item.title}</a>
  {#if chat.readOnlyReasonFor(item.id)}<p class="text-xs text-muted-foreground">{m.console_chat_read_only()}</p>{/if}
  {#if snapshot.generations[item.id]}<span role="status" class="text-xs">{m.console_chat_generating()}</span>{/if}
{/snippet}
{#snippet actionsCell(context: DataTableCellContext<ConsoleConversation>)}<ConsoleChatActions
    conversation={context.row.original} />{/snippet}
<div class="conversations-shell">
  <div class={['route-page conversations-page', { 'route-page-empty': empty }]}>
    <PageHeader title={m.console_chat_all()} description={m.console_chat_local_history()} actions={pageActions} />
    {#if snapshot.loadError}<div role="alert" class="flex flex-col items-start gap-2">
        <p class="text-destructive">{localizeBackendErrorMessage(snapshot.loadError)}</p>
        <Button variant="outline" onclick={() => void chat.start()}>{m.console_chat_retry()}</Button>
      </div>
    {:else if snapshot.storageError}<p role="alert" class="text-destructive">
        {m.console_chat_storage_error()}
        {localizeBackendErrorMessage(snapshot.storageError)}
      </p>{/if}
    {#if snapshot.catalogError}<div role="alert" class="flex flex-col items-start gap-2">
        <p class="text-destructive">{localizeBackendErrorMessage(snapshot.catalogError)}</p>
        <Button variant="outline" onclick={() => void chat.refreshCatalog()}>{m.console_chat_refresh()}</Button>
      </div>{/if}
    {#if snapshot.loading}<p role="status">{m.console_chat_loading()}</p>
    {:else if empty}
      <section class="route-section" aria-labelledby="conversations-empty-title">
        <Empty.Root class="flex-1"
          ><Empty.Header
            ><Empty.Title><h2 id="conversations-empty-title">{m.console_chat_empty()}</h2></Empty.Title
            ><Empty.Description>{m.console_chat_empty_description()}</Empty.Description></Empty.Header
          ><Empty.Content><Button href={resolve('/')}>{m.console_chat_start_new()}</Button></Empty.Content></Empty.Root>
      </section>
    {:else if snapshot.conversations.length}
      <section class="conversations-workspace" aria-labelledby="conversations-list-title">
        <h2 id="conversations-list-title" class="sr-only">{m.console_chat_all()}</h2>
        <InputGroup.Root class="conversations-search">
          <InputGroup.Input
            id="conversation-search"
            type="search"
            bind:value={query}
            aria-label={m.console_chat_search()}
            placeholder={m.console_chat_search()} />
          <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
        </InputGroup.Root>
        {#if !conversations.length}
          <p role="status" class="text-sm text-muted-foreground">{m.console_chat_no_results()}</p>
        {:else}
          <div class="route-desktop-table">
            <DataTable
              data={conversations}
              {columns}
              {labels}
              workbench
              stickyHeader
              class="flex-1"
              ariaLabel={m.console_chat_all()}
              getRowId={(item: ConsoleConversation) => item.id}
              onRowClick={({ original, event }: { original: ConsoleConversation; event: MouseEvent }) => {
                if (!(event.target instanceof Element && event.target.closest('a,button,[role="button"]')))
                  void goto(resolve(`?conversation=${encodeURIComponent(original.id)}`))
              }} />
          </div>
          <div class="route-mobile-list">
            {#each conversations as item (item.id)}
              <article class="route-mobile-row conversations-row">
                <div class="conversations-row-body">
                  <a
                    class="block font-medium break-words hover:underline"
                    href={resolve(`?conversation=${encodeURIComponent(item.id)}`)}>{item.title}</a>
                  {#if chat.readOnlyReasonFor(item.id)}
                    <p class="text-xs text-muted-foreground">{m.console_chat_read_only()}</p>
                  {/if}
                  {#if snapshot.generations[item.id]}
                    <p role="status" class="text-xs">{m.console_chat_generating()}</p>
                  {/if}
                  <dl class="conversations-meta">
                    <dt>{m.console_chat_key()}</dt>
                    <dd class="break-words">{item.apiKeyName}</dd>
                    <dt>{m.console_chat_model()}</dt>
                    <dd class="font-technical break-all">{lastModel(item)}</dd>
                    <dt>{m.console_chat_messages()}</dt>
                    <dd class="font-technical">{formatNumber(item.messages.length)}</dd>
                    <dt>{m.console_chat_last_activity()}</dt>
                    <dd class="font-technical">{formatLogTime(item.updatedAt)}</dd>
                  </dl>
                </div>
                <ConsoleChatActions conversation={item} />
              </article>
            {/each}
          </div>
        {/if}
      </section>
    {/if}
  </div>
</div>

<style>
.conversations-shell {
  container: conversations / inline-size;
  min-width: 0;
}

.conversations-workspace {
  display: flex;
  min-width: 0;
  min-height: 0;
  flex: 1;
  flex-direction: column;
  gap: 0.75rem;
}

:global(.conversations-search) {
  width: 100%;
  max-width: 24rem;
}

.conversations-row {
  align-items: start;
}

.conversations-row-body {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.25rem;
}

.conversations-meta {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  gap: 0.25rem 0.75rem;
  margin-top: 0.25rem;
  font-size: 0.875rem;
}

.conversations-meta dt {
  color: var(--muted-foreground);
}

/* 工作台：桌面表格用满窗口剩余高度，长历史在共享表格视口内滚动，末行与操作可达；短历史保持自然高度。 */
@container conversations (min-width: 52.01rem) {
  .conversations-page:not(.route-page-empty) {
    height: max(calc(100svh - 5rem), 32rem);
  }

  .conversations-page .route-desktop-table {
    display: flex;
    min-height: 0;
    flex: 1;
    flex-direction: column;
  }
}
</style>
