<script lang="ts">
import { AlertDialog as AlertDialogPrimitive } from 'bits-ui'
import { buttonVariants, type ButtonVariant, type ButtonSize } from '$lib/components/ui/button/index.js'
import { cn } from '$lib/utils.js'

import { getAlertDialogContext } from './context.js'

let {
  ref = $bindable(null),
  class: className,
  variant = 'default',
  size = 'default',
  onclick,
  ...restProps
}: AlertDialogPrimitive.ActionProps & { variant?: ButtonVariant; size?: ButtonSize } = $props()

const dialog = getAlertDialogContext()

function handleClick(event: Parameters<NonNullable<typeof onclick>>[0]): void {
  onclick?.(event)
  // Bits UI's Action has no built-in close; AlertDialog semantics are confirm + dismiss.
  // Call sites can keep the dialog open via event.preventDefault().
  if (!event.defaultPrevented) dialog?.close()
}
</script>

<AlertDialogPrimitive.Action
  bind:ref
  data-slot="alert-dialog-action"
  class={cn(buttonVariants({ variant, size }), 'cn-alert-dialog-action', className)}
  onclick={handleClick}
  {...restProps} />
