<script lang="ts">
import { AlertDialog as AlertDialogPrimitive } from 'bits-ui'

import { setAlertDialogContext } from './context.js'

let { open = $bindable(false), ...restProps }: AlertDialogPrimitive.RootProps = $props()

setAlertDialogContext({
  close: () => {
    open = false
  },
})
</script>

<AlertDialogPrimitive.Root bind:open {...restProps} />
