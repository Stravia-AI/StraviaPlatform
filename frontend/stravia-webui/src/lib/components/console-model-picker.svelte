<script lang="ts">
import { tick } from 'svelte'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import { getConsoleChat } from '$lib/console-chat.svelte'
import { effectiveModelDisplayName } from '$lib/logical-model'
import type { ConsoleThinkingSelection } from '$lib/console-chat-types'
import type { ThinkingLevel } from '$lib/types'
import * as m from '$lib/paraglide/messages.js'
import { buttonVariants } from '$lib/components/ui/button'
import { Slider } from '$lib/components/ui/slider'
import * as Command from '$lib/components/ui/command'
import * as Popover from '$lib/components/ui/popover'
import * as Select from '$lib/components/ui/select'

const chat = getConsoleChat()
let open = $state(false)
let layer = $state<'model' | 'effort'>('effort')
let highlighted = $state('')
let modelList = $state<HTMLElement | null>(null)
let modelButton = $state<HTMLButtonElement | null>(null)
const snapshot = $derived(chat.snapshot)
const selectedModel = $derived(snapshot.modelCandidates.find((model) => model.id === snapshot.selectedModelId))
const modelName = $derived(selectedModel ? effectiveModelDisplayName(selectedModel) : m.console_chat_choose_model())
// 仅排序 Route 已公布的规范档位，不从模型名字或上游自定义值构造等级。
const order: ThinkingLevel[] = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']
const ordered = $derived(snapshot.thinkingLevels.every((level) => order.includes(level)))
// 「默认」不对应任何具体档位，固定作为滑杆最左端，不推断上游默认值落在哪一档。
const stops = $derived<ConsoleThinkingSelection[]>([
  'default',
  ...order.filter((level) => snapshot.thinkingLevels.includes(level)),
])
const levelLabels: Record<ConsoleThinkingSelection, () => string> = {
  default: m.console_chat_default,
  off: m.console_chat_effort_off,
  minimal: m.console_chat_effort_minimal,
  low: m.console_chat_effort_low,
  medium: m.console_chat_effort_medium,
  high: m.console_chat_effort_high,
  xhigh: m.console_chat_effort_xhigh,
  max: m.console_chat_effort_max,
}
// 档位只在界面上本地化；请求与回答元信息仍使用协议原值。未知值原样展示。
function thinkingLabel(level: string): string {
  return Object.hasOwn(levelLabels, level) ? levelLabels[level as ConsoleThinkingSelection]() : level
}
const effortLabel = $derived(thinkingLabel(snapshot.thinkingSelection))
const index = $derived(Math.max(0, stops.indexOf(snapshot.thinkingSelection)))
// 最高档位用 peak 渐变强调；只有一个真实档位时它也是唯一档位，不算「最高」。
const peak = $derived(ordered && stops.length > 2 && index === stops.length - 1)
const hasEffort = $derived(Boolean(selectedModel) && snapshot.thinkingLevels.length > 0)

function chooseEffort(value: string) {
  chat.selectThinking(value as ConsoleThinkingSelection)
}

async function showModels() {
  highlighted = snapshot.selectedModelId ?? ''
  layer = 'model'
  await tick()
  modelList?.focus()
}

async function pickModel(id: string) {
  chat.selectModel(id)
  const model = snapshot.modelCandidates.find((candidate) => candidate.id === id)
  if (!model?.supported_thinking_levels.length) {
    open = false
    return
  }
  layer = 'effort'
  await tick()
  modelButton?.focus()
}
</script>

<Popover.Root
  {open}
  onOpenChange={(next: boolean) => {
    open = next
    if (!next) return
    layer = hasEffort ? 'effort' : 'model'
    highlighted = snapshot.selectedModelId ?? ''
  }}>
  <Popover.Trigger
    aria-label={m.console_chat_model_effort()}
    class={buttonVariants({
      variant: 'ghost',
      class: 'min-w-0 max-w-full flex-1 justify-between rounded-full bg-muted/60 px-4 sm:flex-none dark:bg-muted/50',
    })}>
    <span class="flex min-w-0 items-baseline gap-2">
      <span class="truncate">{modelName}</span>
      {#if selectedModel && snapshot.thinkingSelection !== 'default'}<span class="shrink-0 text-muted-foreground"
          >{effortLabel}</span
        >{/if}
    </span><ChevronDownIcon data-icon="inline-end" class="text-muted-foreground" />
  </Popover.Trigger>
  <Popover.Content
    align="end"
    side="top"
    collisionPadding={16}
    class="w-72 max-w-[calc(100vw-2rem)] gap-2 rounded-xl p-3"
    onOpenAutoFocus={(event: Event) => {
      // 模型列表的键盘导航由 Command 根节点处理，直接打开列表层时把焦点交给它。
      if (layer !== 'model') return
      event.preventDefault()
      modelList?.focus()
    }}>
    {#if layer === 'effort'}
      <div class="flex flex-col items-center">
        <p
          class={[
            'text-base leading-tight font-semibold transition-colors duration-[140ms]',
            peak ? 'text-peak' : 'text-primary',
          ]}
          aria-live="polite">
          {effortLabel}
        </p>
        <button
          bind:this={modelButton}
          type="button"
          class="inline-flex min-h-7 max-w-full items-center gap-0.5 rounded-md px-2 text-xs text-muted-foreground transition-colors duration-[140ms] outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50"
          onclick={() => void showModels()}>
          <span class="sr-only">{m.console_chat_model()}: </span><span class="truncate">{modelName}</span
          ><ChevronRightIcon class="size-3.5 shrink-0" aria-hidden="true" />
        </button>
      </div>
      {#if ordered}
        <Slider
          id="chat-thinking"
          size="lg"
          rangeClass={peak ? 'bg-linear-to-r from-primary to-peak' : undefined}
          aria-label={m.console_chat_effort()}
          aria-valuetext={effortLabel}
          min={0}
          max={stops.length - 1}
          step={1}
          bind:value={() => index, (value: number) => chooseEffort(stops[value])} />
      {:else}
        <Select.Root type="single" value={snapshot.thinkingSelection} onValueChange={chooseEffort}>
          <Select.Trigger id="chat-thinking" aria-label={m.console_chat_effort()} class="w-full"
            >{effortLabel}</Select.Trigger>
          <Select.Content
            ><Select.Group>
              {#each ['default', ...snapshot.thinkingLevels] as level (level)}<Select.Item
                  value={level}
                  label={thinkingLabel(level)}>{thinkingLabel(level)}</Select.Item
                >{/each}
            </Select.Group></Select.Content>
        </Select.Root>
      {/if}
    {:else}
      <Command.Root
        bind:ref={modelList}
        bind:value={highlighted}
        shouldFilter={false}
        label={m.console_chat_model()}
        tabindex={0}
        class="bg-transparent p-0 outline-none focus-visible:ring-3 focus-visible:ring-ring/50">
        <Command.List class="max-h-80" aria-label={m.console_chat_model()}>
          {#each snapshot.modelCandidates as model (model.id)}
            {@const name = effectiveModelDisplayName(model)}
            <Command.Item
              value={model.id}
              data-checked={model.id === snapshot.selectedModelId}
              class="min-h-10 rounded-md px-3"
              onSelect={() => void pickModel(model.id)}>
              <span class="flex min-w-0 flex-col"
                ><span class="truncate">{name}</span>
                {#if name !== model.model_id}<span class="truncate font-technical text-xs text-muted-foreground"
                    >{model.model_id}</span
                  >{/if}
              </span>
            </Command.Item>
          {/each}
        </Command.List>
      </Command.Root>
    {/if}
  </Popover.Content>
</Popover.Root>
