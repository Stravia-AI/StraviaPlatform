<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { renderSnippet } from '@tanstack/svelte-table'
import { getConsoleChat } from '$lib/console-chat.svelte'
import type { ConsoleConversation } from '$lib/console-chat-types'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatLogTime, formatNumber } from '$lib/format'
import { getDataTableLabels } from '$lib/data-table-labels'
import * as m from '$lib/paraglide/messages.js'
import ConsoleChatActions from '$lib/components/console-chat-actions.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import { DataTable, createDataTableColumnHelper, type DataTableCellContext } from '$lib/components/ui/data-table'
const chat = getConsoleChat()
let query = $state('')
const snapshot = $derived(chat.snapshot)
const conversations = $derived.by(() => {
  void snapshot.conversations
  return chat.findConversations(query)
})
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<ConsoleConversation>()
const columns = helper.columns([
  helper.accessor('title', {
    header: () => m.console_chat_title(),
    cell: (context) => renderSnippet(titleCell, context),
    meta: { label: () => m.console_chat_title() },
    size: 260,
  }),
  helper.accessor('apiKeyName', {
    header: () => m.console_chat_key(),
    meta: { label: () => m.console_chat_key() },
    size: 160,
  }),
  helper.accessor((item) => lastModel(item), {
    id: 'model',
    header: () => m.console_chat_model(),
    meta: { label: () => m.console_chat_model(), cellClass: 'font-technical text-xs' },
    size: 180,
  }),
  helper.accessor((item) => item.messages.length, {
    id: 'messages',
    header: () => m.console_chat_messages(),
    meta: { label: () => m.console_chat_messages(), align: 'end', cellClass: 'font-technical' },
    size: 90,
  }),
  helper.accessor('updatedAt', {
    header: () => m.console_chat_last_activity(),
    cell: (context) => formatLogTime(context.getValue()),
    meta: { label: () => m.console_chat_last_activity(), cellClass: 'font-technical text-xs' },
    size: 180,
  }),
  helper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(actionsCell, context),
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end' },
    size: 180,
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
  <a href={resolve(`/?conversation=${encodeURIComponent(item.id)}`)} class="font-medium hover:underline"
    >{item.title}</a>
  {#if chat.readOnlyReasonFor(item.id)}<p class="text-xs text-muted-foreground">{m.console_chat_read_only()}</p>{/if}
  {#if snapshot.generations[item.id]}<span role="status" class="text-xs">{m.console_chat_generating()}</span>{/if}
{/snippet}
{#snippet actionsCell(context: DataTableCellContext<ConsoleConversation>)}<ConsoleChatActions
    conversation={context.row.original} />{/snippet}
<div class="route-page">
  <PageHeader
    eyebrow={m.console_chat_chat()}
    title={m.console_chat_all()}
    description={m.console_chat_local_history()}
    actions={pageActions} />
  {#if snapshot.loadError}<p role="alert" class="text-destructive">{localizeBackendErrorMessage(snapshot.loadError)}</p>
    <Button variant="outline" onclick={() => void chat.start()}>{m.console_chat_retry()}</Button>
  {:else if snapshot.storageError}<p role="alert" class="text-destructive">
      {m.console_chat_storage_error()}
      {localizeBackendErrorMessage(snapshot.storageError)}
    </p>{/if}
  {#if snapshot.catalogError}<div role="alert">
      <p class="text-destructive">{localizeBackendErrorMessage(snapshot.catalogError)}</p>
      <Button variant="outline" onclick={() => void chat.refreshCatalog()}>{m.console_chat_refresh()}</Button>
    </div>{/if}
  <Field.FieldGroup
    ><Field.Field orientation="vertical" size="fill"
      ><Field.FieldLabel for="conversation-search">{m.console_chat_search()}</Field.FieldLabel><Input
        id="conversation-search"
        type="search"
        bind:value={query} /></Field.Field
    ></Field.FieldGroup>
  {#if snapshot.loading}<p role="status">{m.console_chat_loading()}</p>
  {:else if !snapshot.conversations.length && !snapshot.loadError}
    <Empty.Root class="border"
      ><Empty.Header
        ><Empty.Title><h2>{m.console_chat_empty()}</h2></Empty.Title><Empty.Description
          >{m.console_chat_empty_description()}</Empty.Description
        ></Empty.Header
      ><Empty.Content><Button href={resolve('/')}>{m.console_chat_start_new()}</Button></Empty.Content></Empty.Root>
  {:else if !conversations.length}<p role="status">{m.console_chat_no_results()}</p>
  {:else}
    <div class="route-desktop-table">
      <DataTable
        data={conversations}
        {columns}
        {labels}
        getRowId={(item: ConsoleConversation) => item.id}
        onRowClick={({ original, event }: { original: ConsoleConversation; event: MouseEvent }) => {
          if (!(event.target instanceof Element && event.target.closest('a,button,[role="button"]')))
            void goto(resolve(`/?conversation=${encodeURIComponent(original.id)}`))
        }} />
    </div>
    <div class="route-mobile-list">
      {#each conversations as item (item.id)}
        <article class="flex flex-col gap-3 border-b py-4">
          <a
            class="block min-h-10 font-medium break-words hover:underline"
            href={resolve(`/?conversation=${encodeURIComponent(item.id)}`)}>{item.title}</a>
          {#if chat.readOnlyReasonFor(item.id)}<p class="text-sm text-muted-foreground">
              {m.console_chat_read_only()}
            </p>{/if}
          {#if snapshot.generations[item.id]}<p role="status">{m.console_chat_generating()}</p>{/if}
          <dl class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-sm">
            <dt class="text-muted-foreground">{m.console_chat_key()}</dt>
            <dd class="break-words">{item.apiKeyName}</dd>
            <dt class="text-muted-foreground">{m.console_chat_model()}</dt>
            <dd class="font-technical break-all">{lastModel(item)}</dd>
            <dt class="text-muted-foreground">{m.console_chat_messages()}</dt>
            <dd class="font-technical">{formatNumber(item.messages.length)}</dd>
            <dt class="text-muted-foreground">{m.console_chat_last_activity()}</dt>
            <dd class="font-technical">{formatLogTime(item.updatedAt)}</dd>
          </dl>
          <ConsoleChatActions conversation={item} />
        </article>
      {/each}
    </div>
  {/if}
</div>
