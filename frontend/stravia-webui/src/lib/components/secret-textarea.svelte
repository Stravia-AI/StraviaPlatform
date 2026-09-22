<script lang="ts">
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import type { HTMLTextareaAttributes } from 'svelte/elements'

import * as m from '$lib/paraglide/messages.js'
import { cn } from '$lib/utils.js'
import * as InputGroup from '$lib/components/ui/input-group'

type Props = HTMLTextareaAttributes & {
  ref?: HTMLTextAreaElement | null
  resetKey?: unknown
  showLabel?: string
  hideLabel?: string
}

let {
  value = $bindable(),
  ref = $bindable(null),
  resetKey,
  showLabel,
  hideLabel,
  disabled,
  class: className,
  ...props
}: Props = $props()
let revealed = $state(false)

$effect(() => {
  void resetKey
  revealed = false
})
</script>

<InputGroup.Root>
  <InputGroup.Textarea
    {...props}
    bind:value
    bind:ref
    {disabled}
    class={cn('secret-textarea min-h-28 font-technical', className)}
    data-revealed={revealed} />
  <InputGroup.Addon align="block-end" class="justify-end">
    <InputGroup.Button
      type="button"
      size="icon-sm"
      {disabled}
      aria-label={revealed ? (hideLabel ?? m.common_hide_secret()) : (showLabel ?? m.common_show_secret())}
      aria-pressed={revealed}
      aria-controls={props.id}
      onclick={() => (revealed = !revealed)}>
      {#if revealed}<EyeOffIcon />{:else}<EyeIcon />{/if}
    </InputGroup.Button>
  </InputGroup.Addon>
</InputGroup.Root>

<style>
:global(.secret-textarea:not([data-revealed='true'])) {
  color: transparent;
  caret-color: var(--foreground);
}

@supports (-webkit-text-security: disc) {
  :global(.secret-textarea:not([data-revealed='true'])) {
    color: inherit;
    -webkit-text-security: disc;
  }
}
</style>
