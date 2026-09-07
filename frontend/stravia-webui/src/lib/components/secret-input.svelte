<script lang="ts">
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import type { ComponentProps, Snippet } from 'svelte'

import * as m from '$lib/paraglide/messages.js'
import { Input } from '$lib/components/ui/input'
import * as InputGroup from '$lib/components/ui/input-group'

type Props = Omit<ComponentProps<typeof Input>, 'type' | 'files'> & {
  resetKey?: unknown
  showLabel?: string
  hideLabel?: string
  actions?: Snippet
}

let {
  value = $bindable(),
  ref = $bindable(null),
  resetKey,
  showLabel,
  hideLabel,
  actions,
  disabled,
  ...props
}: Props = $props()
let revealed = $state(false)

// 切换实体或编辑流程时隐藏值，不改动调用方的草稿。
$effect(() => {
  void resetKey
  revealed = false
})
</script>

<InputGroup.Root>
  <InputGroup.Input {...props} bind:value bind:ref {disabled} type={revealed ? 'text' : 'password'} />
  <InputGroup.Addon align="inline-end">
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
    {@render actions?.()}
  </InputGroup.Addon>
</InputGroup.Root>
