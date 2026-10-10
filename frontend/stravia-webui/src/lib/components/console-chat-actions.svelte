<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { toast } from 'svelte-sonner'
import { getConsoleChat } from '$lib/console-chat.svelte'
import type { ConsoleConversation } from '$lib/console-chat-types'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import * as m from '$lib/paraglide/messages.js'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import * as Dialog from '$lib/components/ui/dialog'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import * as Field from '$lib/components/ui/field'
let { conversation = null, clearAll = false }: { conversation?: ConsoleConversation | null; clearAll?: boolean } =
  $props()
const chat = getConsoleChat()
let renameOpen = $state(false)
let deleteOpen = $state(false)
let title = $state('')
let busy = $state(false)
let error = $state('')
async function rename() {
  if (!conversation || !title.trim()) return
  busy = true
  error = ''
  try {
    await chat.rename(conversation.id, title.trim())
    renameOpen = false
    toast.success(m.console_chat_renamed())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    busy = false
  }
}
async function remove() {
  busy = true
  error = ''
  try {
    const current = chat.snapshot.currentConversationId
    if (clearAll) await chat.clearAll()
    else if (conversation) await chat.delete(conversation.id)
    deleteOpen = false
    if (page.url.pathname === '/' && (clearAll || current === conversation?.id)) await goto(resolve('/'))
    toast.success(m.console_chat_deleted())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    busy = false
  }
}
</script>

<div class="flex flex-wrap items-center gap-1">
  {#if clearAll}
    <Button
      variant="outline"
      disabled={!chat.snapshot.conversations.length}
      onclick={() => {
        error = ''
        deleteOpen = true
      }}>{m.console_chat_clear()}</Button>
  {:else if conversation}
    <Button
      variant="ghost"
      onclick={() => {
        title = conversation?.title ?? ''
        error = ''
        renameOpen = true
      }}>{m.console_chat_rename()}</Button>
    <Button
      variant="ghost"
      onclick={() => {
        error = ''
        deleteOpen = true
      }}>{m.console_chat_delete()}</Button>
  {/if}
</div>
<Dialog.Root bind:open={renameOpen}>
  <Dialog.Content>
    <Dialog.Header
      ><Dialog.Title>{m.console_chat_rename_title()}</Dialog.Title><Dialog.Description
        >{m.console_chat_rename_description()}</Dialog.Description
      ></Dialog.Header>
    <form
      class="flex flex-col gap-4"
      onsubmit={(event) => {
        event.preventDefault()
        void rename()
      }}>
      <Field.FieldGroup
        ><Field.Field orientation="vertical">
          <Field.FieldLabel for={`conversation-title-${conversation?.id}`}>{m.console_chat_title()}</Field.FieldLabel>
          <Input id={`conversation-title-${conversation?.id}`} bind:value={title} disabled={busy} required />
        </Field.Field></Field.FieldGroup>
      {#if error}<p role="alert" class="text-destructive">{error}</p>{/if}
      <Dialog.Footer
        ><Button type="button" variant="outline" disabled={busy} onclick={() => (renameOpen = false)}
          >{m.common_cancel()}</Button
        ><Button type="submit" disabled={busy || !title.trim()} aria-busy={busy}>{m.console_chat_rename()}</Button
        ></Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>
<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{clearAll ? m.console_chat_clear_title() : m.console_chat_delete_title()}</AlertDialog.Title>
      <AlertDialog.Description
        >{clearAll
          ? m.console_chat_clear_description({ count: chat.snapshot.conversations.length })
          : m.console_chat_delete_description({ title: conversation?.title ?? '' })}</AlertDialog.Description>
    </AlertDialog.Header>
    {#if error}<p role="alert" class="text-destructive">{error}</p>{/if}
    <AlertDialog.Footer
      ><AlertDialog.Cancel disabled={busy}>{m.common_cancel()}</AlertDialog.Cancel><Button
        variant="destructive"
        disabled={busy}
        aria-busy={busy}
        onclick={() => void remove()}>{clearAll ? m.console_chat_clear() : m.console_chat_delete()}</Button
      ></AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
