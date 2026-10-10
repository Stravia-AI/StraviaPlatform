<script lang="ts">
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left'
import CheckIcon from '@lucide/svelte/icons/check'
import { getConsoleChat } from '$lib/console-chat.svelte'
import { effectiveModelDisplayName } from '$lib/logical-model'
import type { ConsoleThinkingSelection } from '$lib/console-chat-types'
import type { ThinkingLevel } from '$lib/types'
import * as m from '$lib/paraglide/messages.js'
import { Button, buttonVariants } from '$lib/components/ui/button'
import { Slider } from '$lib/components/ui/slider'
import * as Field from '$lib/components/ui/field'
import * as Popover from '$lib/components/ui/popover'
import * as Select from '$lib/components/ui/select'

const chat = getConsoleChat()
let open = $state(false)
let layer = $state<'model' | 'effort'>('model')
const snapshot = $derived(chat.snapshot)
const selectedModel = $derived(snapshot.modelCandidates.find((model) => model.id === snapshot.selectedModelId))
// 仅排序 Route 已公布的规范档位，不从模型名字或上游自定义值构造等级。
const order: ThinkingLevel[] = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']
const levels = $derived(order.filter((level) => snapshot.thinkingLevels.includes(level)))
const ordered = $derived(snapshot.thinkingLevels.every((level) => order.includes(level)))
const effortLabel = $derived(
  snapshot.thinkingSelection === 'default' ? m.console_chat_default() : snapshot.thinkingSelection,
)
const index = $derived(Math.max(0, levels.indexOf(snapshot.thinkingSelection as ThinkingLevel)))
function chooseEffort(value: string) {
  chat.selectThinking(value as ConsoleThinkingSelection)
}
</script>

<Popover.Root
  {open}
  onOpenChange={(next: boolean) => {
    open = next
    if (next) layer = 'model'
  }}>
  <Popover.Trigger
    aria-label={m.console_chat_model_effort()}
    class={buttonVariants({ variant: 'ghost', class: 'h-auto min-w-0 max-w-full flex-1 py-1 sm:flex-none' })}>
    <span class="min-w-0 flex-1 text-left sm:flex sm:items-center sm:gap-2">
      <span class="block truncate"
        >{selectedModel ? effectiveModelDisplayName(selectedModel) : m.console_chat_choose_model()}</span>
      <span class="block shrink-0 text-xs text-muted-foreground sm:text-sm">{effortLabel}</span>
    </span><ChevronDownIcon data-icon="inline-end" />
  </Popover.Trigger>
  <Popover.Content align="end" side="top" class="w-80 max-w-[calc(100vw-2rem)]">
    {#if layer === 'model'}
      <Popover.Header><Popover.Title>{m.console_chat_model()}</Popover.Title></Popover.Header>
      <Field.FieldGroup>
        <Field.Field orientation="vertical">
          <Field.FieldLabel for="chat-model">{m.console_chat_model()}</Field.FieldLabel>
          <Select.Root
            type="single"
            value={snapshot.selectedModelId ?? ''}
            onValueChange={(value: string) => chat.selectModel(value || null)}>
            <Select.Trigger id="chat-model" class="w-full"
              >{selectedModel
                ? effectiveModelDisplayName(selectedModel)
                : m.console_chat_choose_model()}</Select.Trigger>
            <Select.Content
              ><Select.Group>
                {#each snapshot.modelCandidates as model (model.id)}
                  <Select.Item value={model.id} label={effectiveModelDisplayName(model)}>
                    <span class="flex min-w-0 flex-col"
                      ><span>{effectiveModelDisplayName(model)}</span>
                      {#if effectiveModelDisplayName(model) !== model.model_id}<span
                          class="font-technical text-xs text-muted-foreground">{model.model_id}</span
                        >{/if}
                    </span>
                  </Select.Item>
                {/each}
              </Select.Group></Select.Content>
          </Select.Root>
        </Field.Field>
      </Field.FieldGroup>
      <Button
        variant="ghost"
        class="mt-3 w-full justify-between"
        disabled={!selectedModel}
        onclick={() => (layer = 'effort')}>
        <span>{m.console_chat_effort()}</span><span>{effortLabel}</span>
      </Button>
    {:else}
      <Button variant="ghost" onclick={() => (layer = 'model')}
        ><ArrowLeftIcon data-icon="inline-start" />{m.console_chat_model()}</Button>
      <Field.FieldGroup>
        <Field.Field orientation="vertical">
          <Field.FieldLabel for="chat-thinking">{m.console_chat_effort()}: {effortLabel}</Field.FieldLabel>
          <Button variant="outline" class="w-full justify-between" onclick={() => chooseEffort('default')}>
            {m.console_chat_default()}{#if snapshot.thinkingSelection === 'default'}<CheckIcon
                data-icon="inline-end" />{/if}
          </Button>
          {#if ordered && levels.length > 1}
            <Slider
              id="chat-thinking"
              aria-label={m.console_chat_effort()}
              min={0}
              max={levels.length - 1}
              step={1}
              bind:value={() => index, (value: number) => chooseEffort(levels[value])}
              class="my-3" />
            <div class="flex flex-wrap justify-between gap-2 text-xs text-muted-foreground">
              {#each levels as level (level)}<span>{level}</span>{/each}
            </div>
          {:else if snapshot.thinkingLevels.length}
            <Select.Root type="single" value={snapshot.thinkingSelection} onValueChange={chooseEffort}>
              <Select.Trigger id="chat-thinking" class="w-full">{effortLabel}</Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each snapshot.thinkingLevels as level (level)}<Select.Item value={level} label={level}
                      >{level}</Select.Item
                    >{/each}
                </Select.Group></Select.Content>
            </Select.Root>
          {/if}
        </Field.Field>
      </Field.FieldGroup>
    {/if}
  </Popover.Content>
</Popover.Root>
