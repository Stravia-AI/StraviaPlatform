<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import { toast } from 'svelte-sonner'

import { cn } from '$lib/utils'
import { Button } from '$lib/components/ui/button'
import * as Tooltip from '$lib/components/ui/tooltip'

interface Props {
  value: string
  display?: string
  copyable?: boolean
  class?: string
}

let { value, display = value, copyable = false, class: className }: Props = $props()
let copied = $state(false)
let copiedTimer: ReturnType<typeof setTimeout> | undefined

async function copyValue(): Promise<void> {
  try {
    await navigator.clipboard.writeText(value)
    copied = true
    clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => (copied = false), 1200)
    toast.success(m.common_copied_clipboard())
  } catch {
    toast.error(m.common_not_copy_clipboard())
  }
}
</script>

<span class="flex min-w-0 items-center gap-1">
  <Tooltip.Root>
    <Tooltip.Trigger
      class={cn(
        'font-technical flex min-h-10 min-w-0 items-center truncate text-left text-xs tabular-nums transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)]',
        copied && 'text-signal',
        className,
      )}
      >{display}</Tooltip.Trigger>
    <Tooltip.Content class="max-w-[min(32rem,calc(100vw-2rem))] break-all font-mono text-xs">{value}</Tooltip.Content>
  </Tooltip.Root>
  {#if copyable}
    <Button
      size="icon-sm"
      variant="ghost"
      onclick={() => void copyValue()}
      aria-label={m.technical_value_copy_full_value()}>
      <ClipboardCopyIcon />
    </Button>
  {/if}
</span>
