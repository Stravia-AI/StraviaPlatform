<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { onMount, tick, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { isTauri } from '$lib/admin-client'
import { getConsoleChat } from '$lib/console-chat.svelte'
import { consoleAssistantContent, consoleReasoningActivities, consoleVisibleText } from '$lib/console-chat'
import type {
  ConsoleChatSnapshot,
  ConsoleConversation,
  ConsoleImageAttachment,
  ConsoleTokenUsage,
} from '$lib/console-chat-types'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatLogTime, formatNumber } from '$lib/format'
import * as m from '$lib/paraglide/messages.js'
import ConsoleChatActions from '$lib/components/console-chat-actions.svelte'
import ConsoleChatComposer from '$lib/components/console-chat-composer.svelte'
import ConversationMessage from '$lib/components/conversation-message.svelte'
import ObservationActivity from '$lib/components/observation-activity.svelte'
import DesktopPortNotice from '$lib/components/desktop-port-notice.svelte'
import MarkdownContent from '$lib/components/markdown-content.svelte'
import StreamingMarkdown from '$lib/components/streaming-markdown.svelte'
import { Button } from '$lib/components/ui/button'
import { Skeleton } from '$lib/components/ui/skeleton'
import * as Alert from '$lib/components/ui/alert'
import * as Empty from '$lib/components/ui/empty'
const chat = getConsoleChat()
const snapshot: ConsoleChatSnapshot = $derived(chat.snapshot)
const conversation: ConsoleConversation | null = $derived(snapshot.currentConversation)
const generation = $derived(conversation ? snapshot.generations[conversation.id] : undefined)
const lastMessage = $derived(conversation?.messages.at(-1))
const title = $derived(conversation?.title ?? m.console_chat_new())
let text = $state('')
let images = $state<ConsoleImageAttachment[]>([])
let composer = $state<HTMLTextAreaElement | null>(null)
let surface = $state<HTMLElement | null>(null)
let follow = $state(true)
let scroller: HTMLElement | null = null
let previousConversation: string | null = null
let previousAddress: string | null | undefined
let preservedNavigation: string | null | undefined
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
  if (id !== previousAddress) {
    if (id !== preservedNavigation) {
      text = ''
      images = []
    }
    previousAddress = id
    preservedNavigation = undefined
  }
  untrack(() => chat.openConversation(id))
})
$effect(() => {
  const id = snapshot.currentConversationId
  if (id !== previousConversation) {
    previousConversation = id
    follow = true
  }
  const messages = conversation?.messages
  const liveText = generation?.text
  const activities = generation?.activities
  void messages
  void liveText
  void activities
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
    if (surface && scroller) surface.style.setProperty('--chat-workspace-height', `${scroller.clientHeight - 32}px`)
    if (scroller && follow) scroller.scrollTop = scroller.scrollHeight
  })
  if (surface) resize.observe(surface)
  if (scroller) resize.observe(scroller)
  return () => {
    scroller?.removeEventListener('scroll', onScroll)
    resize.disconnect()
  }
})
async function command(action: () => Promise<unknown>) {
  try {
    await action()
  } catch (cause) {
    toast.error(localizeBackendErrorMessage(cause))
  }
}
async function send() {
  if (!text.trim() || generation || !snapshot.selectedKeyId || !snapshot.selectedModelId) return
  const draft = text
  const attachments = images
  follow = true
  try {
    const pending = chat.send(draft, attachments)
    const id = chat.snapshot.currentConversationId
    // 首次发送建立对话身份不是用户切换对话；错误时必须保留该份草稿。
    if (id && page.url.searchParams.get('conversation') !== id) {
      preservedNavigation = id
      await goto(resolve(`/?conversation=${encodeURIComponent(id)}`), { noScroll: true, keepFocus: true })
    }
    const sent = await pending
    if (
      !sent &&
      id &&
      !chat.snapshot.conversations.some((item) => item.id === id) &&
      page.url.searchParams.get('conversation') === id
    ) {
      preservedNavigation = null
      await goto(resolve('/'), { noScroll: true, keepFocus: true })
    }
    if (sent && chat.snapshot.currentConversationId === id) {
      if (text === draft) text = ''
      if (images === attachments) images = []
    }
  } catch (cause) {
    toast.error(localizeBackendErrorMessage(cause))
  }
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
<section bind:this={surface} class="chat-surface">
  {#if isTauri}<DesktopPortNotice />{/if}
  <header class="flex flex-col items-stretch gap-3 border-b pb-4 sm:flex-row sm:items-start sm:justify-between">
    <h1 class="min-w-0 flex-1 font-structural text-[26px] font-semibold break-words sm:text-[30px]">
      {snapshot.missingConversation ? m.console_chat_missing() : title}
    </h1>
    <div class="flex shrink-0 flex-wrap gap-1 self-end sm:self-auto">
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
      <div class="mx-auto flex w-full max-w-4xl flex-col gap-6 pb-6">
        {#each conversation.messages as message (message.id)}
          {#if message.role === 'user'}
            <ConversationMessage user label={m.console_chat_user_message()}>
              <p class="whitespace-pre-wrap">{consoleVisibleText(message.text)}</p>
              {#if message.images?.length}<div class="flex flex-wrap gap-2">
                  {#each message.images as image (image.id)}<img
                      src={image.dataUrl}
                      alt={image.name}
                      class="max-h-72 max-w-full rounded-lg border object-contain" />{/each}
                </div>{/if}
            </ConversationMessage>
          {:else}
            {@const content = consoleAssistantContent(message)}
            {@const usage = message.usage as ConsoleTokenUsage | undefined}
            {@const live = generation && message === lastMessage}
            {@const activities = live ? generation.activities : consoleReasoningActivities(message)}
            <ConversationMessage label={m.console_chat_assistant_response()}>
              {#each activities as activity (activity.id)}<ObservationActivity
                  {activity}
                  expansionPolicy="chat"
                  minimumHeadingLevel={2} />{/each}
              {#if live}<StreamingMarkdown text={consoleVisibleText(generation.text)} active minimumHeadingLevel={2} />
              {:else}<MarkdownContent text={consoleVisibleText(content.text)} minimumHeadingLevel={2} />{/if}
              {#if message.status === 'failed' && !live}<div role="alert" class="text-destructive">
                  {localizeBackendErrorMessage(
                    message.error ?? m.console_chat_request_failed(),
                  )}{#if message === lastMessage && !snapshot.retryModelAvailable}<p class="mt-1 text-sm">
                      {m.console_chat_model_recovery()}
                    </p>{/if}
                </div>{/if}
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
                    disabled={!snapshot.selectedModelId ||
                      Boolean(guide) ||
                      Boolean(snapshot.catalogError) ||
                      (snapshot.historyHasImages && !snapshot.modelSupportsImages)}
                    onclick={() => void command(() => chat.regenerate())}>{m.console_chat_regenerate()}</Button>
                {/if}
              </div>
              {#snippet meta()}
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
              {/snippet}
            </ConversationMessage>
          {/if}
        {/each}
      </div>
    {:else if !guide && !snapshot.loadError && !snapshot.catalogError}
      <Empty.Root class="flex-1"
        ><Empty.Header><Empty.Description>{m.console_chat_start_prompt()}</Empty.Description></Empty.Header
        ></Empty.Root>
    {/if}
    {#if snapshot.readOnlyReason}
      <Alert.Root role="status" class="mx-auto mt-auto max-w-4xl"
        ><Alert.Title>{m.console_chat_read_only()}</Alert.Title><Alert.Description
          ><p>{readOnlyText}</p>
          <Button variant="outline" onclick={() => void newConversation()}>{m.console_chat_other_key()}</Button
          ></Alert.Description
        ></Alert.Root>
    {:else if guide}
      <Empty.Root class="mx-auto my-auto w-full max-w-xl border" aria-labelledby="chat-setup-title"
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
      <div class="composer-dock">
        {#if !follow && conversation}<Button
            class="mx-auto"
            variant="outline"
            onclick={() => {
              follow = true
              if (scroller) scroller.scrollTop = scroller.scrollHeight
            }}>{m.console_chat_latest()}</Button
          >{/if}
        <ConsoleChatComposer
          bind:text
          bind:images
          bind:composer
          onsend={send}
          onstop={() => void command(() => chat.stop())} />
      </div>
    {/if}
    {#if generation && (guide || snapshot.catalogError || snapshot.readOnlyReason)}<Button
        variant="outline"
        onclick={() => void command(() => chat.stop())}>{m.console_chat_stop()}</Button
      >{/if}
  {/if}
</section>

<style>
.chat-surface {
  display: flex;
  min-width: 0;
  min-height: var(--chat-workspace-height, calc(100svh - 5rem));
  flex-direction: column;
  gap: 1.5rem;
}
.composer-dock {
  position: sticky;
  bottom: -1rem;
  display: flex;
  width: 100%;
  max-width: 56rem;
  min-width: 0;
  flex-direction: column;
  gap: 0.5rem;
  align-self: center;
  margin-top: auto;
  padding-block: 0.75rem max(1rem, env(safe-area-inset-bottom));
  background: var(--background);
}
</style>
