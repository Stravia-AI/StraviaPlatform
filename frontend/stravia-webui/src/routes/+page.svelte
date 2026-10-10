<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { onMount, tick, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { isTauri } from '$lib/admin-client'
import { getConsoleChat } from '$lib/console-chat.svelte'
import { consoleAssistantContent, consoleVisibleText } from '$lib/console-chat'
import type {
  ConsoleChatSnapshot,
  ConsoleConversation,
  ConsoleThinkingSelection,
  ConsoleTokenUsage,
} from '$lib/console-chat-types'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { effectiveModelDisplayName } from '$lib/logical-model'
import { formatLogTime, formatNumber } from '$lib/format'
import * as m from '$lib/paraglide/messages.js'
import ConsoleChatActions from '$lib/components/console-chat-actions.svelte'
import DesktopPortNotice from '$lib/components/desktop-port-notice.svelte'
import MarkdownContent from '$lib/components/markdown-content.svelte'
import StreamingMarkdown from '$lib/components/streaming-markdown.svelte'
import { Button } from '$lib/components/ui/button'
import { Skeleton } from '$lib/components/ui/skeleton'
import { Textarea } from '$lib/components/ui/textarea'
import * as Alert from '$lib/components/ui/alert'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
const chat = getConsoleChat()
const snapshot: ConsoleChatSnapshot = $derived(chat.snapshot)
const conversation: ConsoleConversation | null = $derived(snapshot.currentConversation)
const generation = $derived(conversation ? snapshot.generations[conversation.id] : undefined)
const lastMessage = $derived(conversation?.messages.at(-1))
const title = $derived(conversation?.title ?? m.console_chat_new())
const keyOptions = $derived(snapshot.keyCandidates.map((key) => ({ value: key.id, label: key.name })))
const modelOptions = $derived(
  snapshot.modelCandidates.map((model) => {
    const name = effectiveModelDisplayName(model)
    return { value: model.id, label: name === model.model_id ? name : `${name} (${model.model_id})` }
  }),
)
const effortOptions = $derived([
  { value: 'default', label: m.console_chat_default() },
  ...snapshot.thinkingLevels.map((level) => ({ value: level, label: level })),
])
let text = $state('')
let composer = $state<HTMLTextAreaElement | null>(null)
let surface = $state<HTMLElement | null>(null)
let follow = $state(true)
let scroller: HTMLElement | null = null
let previousConversation: string | null = null
const guide = $derived.by(() => {
  switch (snapshot.blocker) {
    case 'no-services':
      return {
        title: m.console_chat_connect_service_title(),
        description: m.console_chat_connect_service_description(),
        label: m.common_connect_model_service(),
        href: '/providers' as const,
      }
    case 'disabled-services':
      return {
        title: m.console_chat_enable_service_title(),
        description: m.console_chat_enable_service_description(),
        label: m.console_chat_review_model_services(),
        href: '/providers' as const,
      }
    case 'no-models':
      return {
        title: m.console_chat_add_model_title(),
        description: m.console_chat_add_model_description(),
        label: m.common_add_model(),
        href: '/models' as const,
      }
    case 'disabled-models':
      return {
        title: m.console_chat_review_models_title(),
        description: m.console_chat_review_models_description(),
        label: m.console_chat_review_models(),
        href: '/models' as const,
      }
    case 'no-keys':
      return {
        title: m.console_chat_create_api_key_title(),
        description: m.console_chat_create_api_key_description(),
        label: m.common_create_api_key(),
        href: '/api-keys' as const,
      }
    case 'unavailable-keys':
      return {
        title: m.console_chat_review_api_keys_title(),
        description: m.console_chat_review_api_keys_description(),
        label: m.console_chat_review_api_keys(),
        href: '/api-keys' as const,
      }
    default:
      return null
  }
})
const readOnlyText = $derived(
  snapshot.readOnlyReason === 'deleted'
    ? m.console_chat_key_deleted()
    : snapshot.readOnlyReason === 'disabled'
      ? m.console_chat_key_disabled()
      : m.console_chat_key_expired(),
)
$effect(() => {
  const id = page.url.searchParams.get('conversation')
  untrack(() => chat.openConversation(id))
})
$effect(() => {
  const id = snapshot.currentConversationId
  if (id !== previousConversation) {
    previousConversation = id
    follow = true
    text = ''
  }
  const messages = conversation?.messages
  const liveText = generation?.text
  const thinking = generation?.summary || generation?.reasoning
  void messages
  void liveText
  void thinking
  if (untrack(() => follow))
    void tick().then(() => {
      if (scroller && follow) scroller.scrollTop = scroller.scrollHeight
    })
})
onMount(() => {
  scroller = surface?.closest('main') ?? null
  const onScroll = () => {
    if (scroller) follow = scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop < 80
  }
  scroller?.addEventListener('scroll', onScroll, { passive: true })
  const resize = new ResizeObserver(() => {
    if (scroller && follow) scroller.scrollTop = scroller.scrollHeight
  })
  if (surface) resize.observe(surface)
  return () => {
    scroller?.removeEventListener('scroll', onScroll)
    resize.disconnect()
  }
})
async function command(action: () => Promise<void>) {
  try {
    await action()
  } catch (cause) {
    toast.error(localizeBackendErrorMessage(cause))
  }
}
async function send() {
  if (!text.trim() || generation || !snapshot.selectedKeyId || !snapshot.selectedModelId) return
  const draft = text
  follow = true
  const pending = command(() => chat.send(draft))
  const id = chat.snapshot.currentConversationId
  if (id && page.url.searchParams.get('conversation') !== id)
    await goto(resolve(`/?conversation=${encodeURIComponent(id)}`), { noScroll: true, keepFocus: true })
  if (chat.snapshot.currentConversation?.messages.some((message) => message.role === 'user' && message.text === draft))
    text = ''
  await pending
  composer?.focus()
}
async function newConversation() {
  chat.newConversation()
  await goto(resolve('/'))
  composer?.focus()
}
async function copy(value: string) {
  await command(async () => {
    await navigator.clipboard.writeText(consoleVisibleText(value))
    toast.success(m.common_copied_clipboard())
  })
}
</script>

<svelte:head><title>{title} · Stravia</title></svelte:head>
<section bind:this={surface} class="flex min-w-0 flex-col gap-6">
  {#if isTauri}<DesktopPortNotice />{/if}
  <header class="flex flex-wrap items-start justify-between gap-3 border-b pb-4">
    <div class="min-w-0 flex-1">
      <h1 class="font-structural text-[26px] font-semibold break-words sm:text-[30px]">
        {snapshot.missingConversation ? m.console_chat_missing() : title}
      </h1>
      {#if conversation}<p class="mt-2 text-sm text-muted-foreground">
          {m.console_chat_key()}: <span class="text-foreground">{conversation.apiKeyName}</span>
          <span class="text-xs">{m.console_chat_key_locked()}</span>
        </p>{/if}
    </div>
    <div class="flex flex-wrap gap-1">
      <Button variant="outline" onclick={() => void newConversation()}>{m.console_chat_new()}</Button
      ><ConsoleChatActions {conversation} />
    </div>
  </header>
  {#if snapshot.storageError}<p role="alert" class="text-destructive">
      {m.console_chat_storage_error()}
      {localizeBackendErrorMessage(snapshot.storageError)}
    </p>{/if}
  {#if snapshot.loadError || snapshot.catalogError}
    <div role="alert" class="flex flex-col items-start gap-3">
      <p class="text-destructive">{localizeBackendErrorMessage(snapshot.loadError ?? snapshot.catalogError)}</p>
      <Button
        variant="outline"
        onclick={() => void command(() => (snapshot.loadError ? chat.start() : chat.refreshCatalog()))}
        >{m.console_chat_retry()}</Button>
    </div>
  {/if}
  {#if snapshot.loading}<div role="status" aria-label={m.console_chat_loading()} class="flex flex-col gap-4">
      <Skeleton class="h-8 w-48" /><Skeleton class="h-32 w-full" />
    </div>
  {:else if snapshot.missingConversation}
    <Empty.Root
      ><Empty.Header><Empty.Description>{m.console_chat_missing_description()}</Empty.Description></Empty.Header
      ><Empty.Content
        ><Button onclick={() => void newConversation()}>{m.console_chat_start_new()}</Button></Empty.Content
      ></Empty.Root>
  {:else}
    {#if conversation?.messages.length}
      <div class="mx-auto flex w-full max-w-4xl flex-col gap-6">
        {#each conversation.messages as message (message.id)}
          {#if message.role === 'user'}
            <article
              aria-label={m.console_chat_user_message()}
              class="ms-auto max-w-full rounded-lg border bg-muted px-4 py-3 whitespace-pre-wrap break-words">
              {consoleVisibleText(message.text)}
            </article>
          {:else}
            {@const content = consoleAssistantContent(message)}
            {@const usage = message.usage as ConsoleTokenUsage | undefined}
            {@const live = generation && message === lastMessage}
            {@const thinking = consoleVisibleText(live ? generation.summary || generation.reasoning : content.thinking)}
            <article aria-label={m.console_chat_assistant_response()} class="flex min-w-0 flex-col gap-3">
              {#if thinking}<details class="rounded-lg border px-3 py-2">
                  <summary class="min-h-10 cursor-pointer py-2 text-sm text-muted-foreground"
                    >{m.console_chat_reasoning()}</summary
                  ><StreamingMarkdown text={thinking} active={Boolean(live)} minimumHeadingLevel={2} />
                </details>{/if}
              {#if live}<StreamingMarkdown text={consoleVisibleText(generation.text)} active minimumHeadingLevel={2} />
              {:else}<MarkdownContent text={consoleVisibleText(content.text)} minimumHeadingLevel={2} />{/if}
              {#if message.status === 'failed' && !live}<div role="alert" class="text-destructive">
                  {localizeBackendErrorMessage(
                    message.error ?? m.console_chat_request_failed(),
                  )}{#if message === lastMessage && !snapshot.retryModelAvailable}<p class="mt-1 text-sm">
                      {m.console_chat_model_recovery()}
                    </p>{/if}
                </div>{/if}
              <div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
                <span class="font-technical break-all">{message.routeId}</span><span
                  >{m.console_chat_effort()}: {message.thinkingLevel === 'default'
                    ? m.console_chat_default()
                    : message.thinkingLevel}</span
                ><time datetime={message.createdAt} class="font-technical">{formatLogTime(message.createdAt)}</time>
                {#if usage?.inputTokens !== undefined}<span
                    >{m.console_chat_input_tokens({ count: formatNumber(usage.inputTokens) })}</span
                  >{/if}
                {#if usage?.outputTokens !== undefined}<span
                    >{m.console_chat_output_tokens({ count: formatNumber(usage.outputTokens) })}</span
                  >{/if}
                {#if live}<span role="status">{m.console_chat_generating()}</span
                  >{:else if message.status === 'stopped'}<span role="status">{m.console_chat_stopped()}</span
                  >{:else if message.status === 'incomplete'}<span role="status">{m.console_chat_incomplete()}</span
                  >{:else if message.status === 'completed'}<span role="status">{m.console_chat_completed()}</span>{/if}
              </div>
              <div class="flex flex-wrap gap-1">
                <Button variant="ghost" onclick={() => void copy(live ? generation.text : content.text)}
                  >{m.console_chat_copy()}</Button>
                {#if message === lastMessage && !generation && !snapshot.readOnlyReason}
                  {#if message.status === 'failed'}<Button
                      variant="outline"
                      disabled={!snapshot.retryModelAvailable || Boolean(guide) || Boolean(snapshot.catalogError)}
                      onclick={() => void command(() => chat.retry())}>{m.console_chat_retry()}</Button
                    >{/if}
                  <Button
                    variant="ghost"
                    disabled={!snapshot.selectedModelId || Boolean(guide) || Boolean(snapshot.catalogError)}
                    onclick={() => void command(() => chat.regenerate())}>{m.console_chat_regenerate()}</Button>
                {/if}
              </div>
            </article>
          {/if}
        {/each}
      </div>
    {/if}
    {#if snapshot.readOnlyReason}
      <Alert.Root role="status" class="mx-auto max-w-4xl"
        ><Alert.Title>{m.console_chat_read_only()}</Alert.Title><Alert.Description
          ><p>{readOnlyText}</p>
          <Button variant="outline" onclick={() => void newConversation()}>{m.console_chat_other_key()}</Button
          ></Alert.Description
        ></Alert.Root>
    {:else if guide}
      <Empty.Root class="mx-auto w-full max-w-xl border" aria-labelledby="chat-setup-title"
        ><Empty.Header
          ><Empty.Title><h2 id="chat-setup-title">{guide.title}</h2></Empty.Title><Empty.Description
            >{guide.description}</Empty.Description
          ></Empty.Header
        ><Empty.Content
          ><Button href={resolve(guide.href)}>{guide.label}</Button><Button
            variant="ghost"
            onclick={() => void command(() => chat.refreshCatalog())}>{m.console_chat_refresh()}</Button
          ></Empty.Content
        ></Empty.Root>
    {:else if !snapshot.loadError && !snapshot.catalogError}
      <form
        class={[
          'mx-auto flex w-full max-w-4xl flex-col gap-3 rounded-xl border bg-card p-4',
          !conversation ? 'my-8 sm:my-16' : 'sticky bottom-0',
        ]}
        onsubmit={(event) => {
          event.preventDefault()
          void send()
        }}>
        <Field.FieldGroup>
          <div class={['grid min-w-0 gap-3', conversation ? 'sm:grid-cols-2' : 'sm:grid-cols-3']}>
            {#if !conversation}
              <Field.Field orientation="vertical">
                <Field.FieldLabel for="chat-key">{m.console_chat_key()}</Field.FieldLabel>
                <Select.Root
                  type="single"
                  value={snapshot.selectedKeyId ?? ''}
                  onValueChange={(value: string) => chat.selectKey(value || null)}>
                  <Select.Trigger id="chat-key" class="w-full"
                    >{keyOptions.find((option) => option.value === snapshot.selectedKeyId)?.label ??
                      m.console_chat_choose_key()}</Select.Trigger>
                  <Select.Content
                    ><Select.Group
                      >{#each keyOptions as option (option.value)}<Select.Item value={option.value} label={option.label}
                          >{option.label}</Select.Item
                        >{/each}</Select.Group
                    ></Select.Content>
                </Select.Root>
              </Field.Field>
            {/if}
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="chat-model">{m.console_chat_model()}</Field.FieldLabel>
              <Select.Root
                type="single"
                value={snapshot.selectedModelId ?? ''}
                onValueChange={(value: string) => chat.selectModel(value || null)}>
                <Select.Trigger id="chat-model" class="w-full"
                  >{modelOptions.find((option) => option.value === snapshot.selectedModelId)?.label ??
                    m.console_chat_choose_model()}</Select.Trigger>
                <Select.Content
                  ><Select.Group
                    >{#each modelOptions as option (option.value)}<Select.Item value={option.value} label={option.label}
                        >{option.label}</Select.Item
                      >{/each}</Select.Group
                  ></Select.Content>
              </Select.Root>
            </Field.Field>
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="chat-thinking">{m.console_chat_effort()}</Field.FieldLabel>
              <Select.Root
                type="single"
                value={snapshot.thinkingSelection}
                onValueChange={(value: string) => chat.selectThinking(value as ConsoleThinkingSelection)}>
                <Select.Trigger id="chat-thinking" class="w-full"
                  >{effortOptions.find((option) => option.value === snapshot.thinkingSelection)?.label}</Select.Trigger>
                <Select.Content
                  ><Select.Group
                    >{#each effortOptions as option (option.value)}<Select.Item
                        value={option.value}
                        label={option.label}>{option.label}</Select.Item
                      >{/each}</Select.Group
                  ></Select.Content>
              </Select.Root>
            </Field.Field>
          </div>
          {#if !snapshot.selectedKeyId || !snapshot.selectedModelId}<p class="text-sm text-muted-foreground">
              {snapshot.selectedKeyId ? m.console_chat_choose_model() : m.console_chat_choose_key()}
            </p>{/if}
          <Field.Field orientation="vertical" data-disabled={Boolean(generation)}>
            <Field.FieldLabel for="chat-message">{m.console_chat_message()}</Field.FieldLabel>
            <Textarea
              bind:ref={composer}
              id="chat-message"
              bind:value={text}
              rows={3}
              class="resize-y"
              disabled={Boolean(generation)}
              placeholder={m.console_chat_message_placeholder()}
              onkeydown={(event: KeyboardEvent) => {
                if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing) {
                  event.preventDefault()
                  void send()
                }
              }} />
          </Field.Field>
        </Field.FieldGroup>
        <div class="flex flex-wrap items-center justify-between gap-3">
          <p class="text-xs text-muted-foreground">{m.console_chat_send_notice()}</p>
          {#if generation}<Button type="button" variant="outline" onclick={() => void command(() => chat.stop())}
              >{m.console_chat_stop()}</Button
            >{:else}<Button
              type="submit"
              disabled={!text.trim() || !snapshot.selectedKeyId || !snapshot.selectedModelId}
              >{m.console_chat_send()}</Button
            >{/if}
        </div>
      </form>
    {/if}
    {#if generation && (guide || snapshot.catalogError || snapshot.readOnlyReason)}<Button
        variant="outline"
        onclick={() => void command(() => chat.stop())}>{m.console_chat_stop()}</Button
      >{/if}
    {#if !follow && conversation}<Button
        class="sticky bottom-4 mx-auto"
        variant="outline"
        onclick={() => {
          follow = true
          if (scroller) scroller.scrollTop = scroller.scrollHeight
        }}>{m.console_chat_latest()}</Button
      >{/if}
  {/if}
</section>
