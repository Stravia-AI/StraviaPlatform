<script lang="ts">
import PlusIcon from '@lucide/svelte/icons/plus'
import XIcon from '@lucide/svelte/icons/x'
import SendIcon from '@lucide/svelte/icons/arrow-up'
import StopIcon from '@lucide/svelte/icons/square'
import { getConsoleChat } from '$lib/console-chat.svelte'
import type { ConsoleImageAttachment } from '$lib/console-chat-types'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import * as m from '$lib/paraglide/messages.js'
import ConsoleModelPicker from '$lib/components/console-model-picker.svelte'
import { Button } from '$lib/components/ui/button'
import { Textarea } from '$lib/components/ui/textarea'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'

let {
  text = $bindable(''),
  images = $bindable<ConsoleImageAttachment[]>([]),
  composer = $bindable<HTMLTextAreaElement | null>(null),
  onsend,
  onstop,
}: {
  text?: string
  images?: ConsoleImageAttachment[]
  composer?: HTMLTextAreaElement | null
  onsend: () => Promise<void>
  onstop: () => void
} = $props()
const chat = getConsoleChat()
const snapshot = $derived(chat.snapshot)
const conversation = $derived(snapshot.currentConversation)
const generating = $derived(Boolean(conversation && snapshot.generations[conversation.id]))
const incompatible = $derived((images.length > 0 || snapshot.historyHasImages) && !snapshot.modelSupportsImages)
let fileInput = $state<HTMLInputElement | null>(null)
let reading = $state(false)
let imageError = $state('')
let dragging = $state(false)
let draftEpoch = 0
let previousId: string | null = null
$effect(() => {
  const id = snapshot.currentConversationId
  if (id !== previousId) {
    previousId = id
    draftEpoch++
    imageError = ''
    dragging = false
  }
})
function readImage(file: File): Promise<ConsoleImageAttachment> {
  if (!['image/png', 'image/jpeg', 'image/webp'].includes(file.type))
    throw new Error(m.console_chat_image_attachment_invalid())
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(reader.error ?? new Error(m.console_chat_image_read_error()))
    reader.onabort = () => reject(new Error(m.console_chat_image_read_error()))
    reader.onload = () => {
      if (typeof reader.result !== 'string') {
        reject(new Error(m.console_chat_image_read_error()))
        return
      }
      resolve({
        id: crypto.randomUUID(),
        name: file.name,
        mediaType: file.type as ConsoleImageAttachment['mediaType'],
        dataUrl: reader.result,
      })
    }
    // 保存原始字节；不用 canvas 重编码，避免损失截图文字和公式细节。
    reader.readAsDataURL(file)
  })
}
async function addFiles(files: File[]) {
  if (!files.length || generating || reading) return
  imageError = ''
  reading = true
  const epoch = draftEpoch
  try {
    const added = await Promise.all(files.map(readImage))
    if (epoch === draftEpoch) images = [...images, ...added]
  } catch (cause) {
    if (epoch === draftEpoch) imageError = localizeBackendErrorMessage(cause)
  } finally {
    reading = false
    if (fileInput) fileInput.value = ''
  }
}
function paste(event: ClipboardEvent) {
  const files = Array.from(event.clipboardData?.files ?? [])
  if (files.length) {
    event.preventDefault()
    void addFiles(files)
  }
}
function drop(event: DragEvent) {
  event.preventDefault()
  dragging = false
  void addFiles(Array.from(event.dataTransfer?.files ?? []))
}
</script>

<div class="flex min-w-0 flex-col gap-2">
  {#if conversation}
    <p class="text-xs text-muted-foreground">
      {m.console_chat_key()}: <span class="text-foreground">{conversation.apiKeyName}</span> · {m.console_chat_key_locked()}
    </p>
  {:else}
    <Field.FieldGroup
      ><Field.Field orientation="vertical">
        <Field.FieldLabel for="chat-key">{m.console_chat_key()}</Field.FieldLabel>
        <Select.Root
          type="single"
          value={snapshot.selectedKeyId ?? ''}
          onValueChange={(value: string) => chat.selectKey(value || null)}>
          <Select.Trigger id="chat-key" class="w-full sm:w-72"
            >{snapshot.keyCandidates.find((key) => key.id === snapshot.selectedKeyId)?.name ??
              m.console_chat_choose_key()}</Select.Trigger>
          <Select.Content
            ><Select.Group
              >{#each snapshot.keyCandidates as key (key.id)}<Select.Item value={key.id} label={key.name}
                  >{key.name}</Select.Item
                >{/each}</Select.Group
            ></Select.Content>
        </Select.Root>
      </Field.Field></Field.FieldGroup>
  {/if}
  <form
    class={['flex min-w-0 flex-col gap-3 rounded-xl border bg-card p-4', dragging && 'outline-2 outline-primary']}
    onsubmit={(event) => {
      event.preventDefault()
      if (!incompatible && !reading) void onsend()
    }}
    ondragover={(event) => {
      if (event.dataTransfer?.types.includes('Files')) {
        event.preventDefault()
        dragging = true
      }
    }}
    ondragleave={(event) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null)) dragging = false
    }}
    ondrop={drop}>
    <Field.FieldGroup>
      <Field.Field orientation="vertical" data-disabled={generating}>
        <Field.FieldLabel for="chat-message" class="sr-only">{m.console_chat_message()}</Field.FieldLabel>
        <Textarea
          bind:ref={composer}
          id="chat-message"
          bind:value={text}
          rows={3}
          class="max-h-56 resize-y"
          disabled={generating}
          placeholder={m.console_chat_message_placeholder()}
          onpaste={paste}
          onkeydown={(event: KeyboardEvent) => {
            if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing) {
              event.preventDefault()
              if (!incompatible && !reading) void onsend()
            }
          }} />
      </Field.Field>
    </Field.FieldGroup>
    {#if images.length}
      <ul aria-label={m.console_chat_images()} class="flex flex-wrap gap-3">
        {#each images as image (image.id)}
          <li class="relative flex w-24 flex-col gap-1">
            <img src={image.dataUrl} alt={image.name} class="h-20 w-full rounded-lg border object-contain" />
            <span class="truncate text-xs text-muted-foreground" title={image.name}>{image.name}</span>
            <Button
              type="button"
              variant="outline"
              size="icon"
              class="absolute top-0 right-0"
              disabled={generating}
              aria-label={m.console_chat_remove_image({ name: image.name })}
              onclick={() => (images = images.filter((item) => item.id !== image.id))}><XIcon /></Button>
          </li>
        {/each}
      </ul>
    {/if}
    {#if imageError}<p role="alert" class="text-sm text-destructive">{imageError}</p>{/if}
    {#if incompatible}<p role="alert" class="text-sm text-destructive">
        {m.console_chat_image_input_unsupported()}
      </p>{/if}
    {#if snapshot.inputError && !incompatible}<p role="alert" class="text-sm text-destructive">
        {snapshot.inputError.code === 'CONSOLE_IMAGE_INPUT_UNSUPPORTED'
          ? m.console_chat_image_input_unsupported()
          : m.console_chat_image_attachment_invalid()}
      </p>{/if}
    {#if !snapshot.selectedKeyId || !snapshot.selectedModelId}<p class="text-sm text-muted-foreground">
        {snapshot.selectedKeyId ? m.console_chat_choose_model() : m.console_chat_choose_key()}
      </p>{/if}
    <div class="flex min-w-0 items-center gap-2">
      <input
        bind:this={fileInput}
        type="file"
        accept="image/png,image/jpeg,image/webp"
        multiple
        class="hidden"
        aria-label={m.console_chat_add_images()}
        onchange={(event) => void addFiles(Array.from(event.currentTarget.files ?? []))} />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        disabled={generating || reading}
        aria-busy={reading}
        aria-label={m.console_chat_add_images()}
        onclick={() => fileInput?.click()}><PlusIcon /></Button>
      <div class="hidden min-w-0 flex-1 sm:block"></div>
      <ConsoleModelPicker />
      {#if generating}
        <Button type="button" variant="outline" size="icon" aria-label={m.console_chat_stop()} onclick={onstop}
          ><StopIcon /></Button>
      {:else}
        <Button
          type="submit"
          size="icon"
          aria-label={m.console_chat_send()}
          disabled={!text.trim() || !snapshot.selectedKeyId || !snapshot.selectedModelId || incompatible || reading}
          ><SendIcon /></Button>
      {/if}
    </div>
  </form>
  <p class="text-xs text-muted-foreground">
    {images.length || snapshot.historyHasImages ? m.console_chat_image_notice() : m.console_chat_send_notice()}
  </p>
</div>
