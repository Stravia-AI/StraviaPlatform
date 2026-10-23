<script lang="ts">
import type { Snippet } from 'svelte'
import CalendarRangeIcon from '@lucide/svelte/icons/calendar-range'

import * as m from '#lib/paraglide/messages.js'
import { formatLogTime } from '#lib/format.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Select from '#lib/components/ui/select/index.js'

interface PresetOption {
  value: string
  label: string
}

interface Props {
  /** 预设选项；label 同时作为下拉项文本。 */
  presets: readonly PresetOption[]
  /** 当前预设值；空串表示自定义范围。 */
  value: string
  selectedLabel: string
  startMs: number
  endMs: number
  live: boolean
  /** 预设下拉的 aria-label。 */
  selectLabel: string
  /** 起止范围按钮的 aria-label。 */
  rangeLabel: string
  /** 较早按钮的 aria-label；可见文案固定为“较早”。 */
  olderLabel: string
  /** 较新按钮的 aria-label；可见文案固定为“较新”。 */
  newerLabel: string
  olderDisabled?: boolean
  newerDisabled?: boolean
  /** 全屏时下拉内容需要挂到工作区节点内。 */
  portalTarget?: HTMLElement
  onpreset: (value: string) => unknown
  onolder: () => unknown
  onnewer: () => unknown
  onrange: () => unknown
  /** 调用方自带的时间条尾部动作（刷新锚点、全屏等）。 */
  trailing?: Snippet
}

let {
  presets,
  value,
  selectedLabel,
  startMs,
  endMs,
  live,
  selectLabel,
  rangeLabel,
  olderLabel,
  newerLabel,
  olderDisabled = false,
  newerDisabled = false,
  portalTarget,
  onpreset,
  onolder,
  onnewer,
  onrange,
  trailing,
}: Props = $props()
</script>

<div class="window-controls">
  <Select.Root type="single" {value} onValueChange={(next: string) => onpreset(next)}>
    <Select.Trigger aria-label={selectLabel} class="w-40 min-w-0">
      <span class="truncate">{selectedLabel}</span>
    </Select.Trigger>
    <Select.Content portalProps={{ to: portalTarget }}>
      <Select.Group>
        {#each presets as preset (preset.value)}
          <Select.Item value={preset.value} label={preset.label}>
            {preset.label}
          </Select.Item>
        {/each}
      </Select.Group>
    </Select.Content>
  </Select.Root>
  <Button variant="ghost" size="sm" disabled={olderDisabled} aria-label={olderLabel} onclick={() => onolder()}
    >{m.observation_older_window()}</Button>
  <Button variant="ghost" size="sm" class="range-picker" aria-label={rangeLabel} onclick={() => onrange()}>
    <CalendarRangeIcon data-icon="inline-start" />
    <span class="range-label">
      {formatLogTime(startMs)} — {live ? m.observation_live() : formatLogTime(endMs)}
    </span>
  </Button>
  <Button variant="ghost" size="sm" disabled={newerDisabled} aria-label={newerLabel} onclick={() => onnewer()}
    >{m.observation_newer_window()}</Button>
  {#if trailing}{@render trailing()}{/if}
</div>

<style>
.window-controls {
  display: flex;
  max-width: 100%;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.3rem;
}
.range-label {
  font-family: var(--font-technical);
  font-size: 0.7rem;
}
@media (max-width: 767px) {
  .window-controls {
    justify-content: space-between;
  }
  .window-controls :global(.range-picker) {
    order: 1;
    width: 100%;
  }
}
</style>
