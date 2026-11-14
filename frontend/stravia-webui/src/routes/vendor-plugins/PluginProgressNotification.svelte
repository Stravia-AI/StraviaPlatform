<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

let { name, stage, bytes, onCancel }: { name: string; stage: string; bytes?: string; onCancel?: () => void } = $props()
</script>

<Alert.Root role="status" class="w-full">
  <Alert.Title>{name}</Alert.Title>
  <Alert.Description>
    <div class="flex items-center gap-2"><Spinner /><span class="tabular-nums">{stage}</span></div>
    {#if bytes}<p class="font-technical tabular-nums">{bytes}</p>{/if}
    {#if onCancel}<Button type="button" size="sm" variant="outline" onclick={onCancel}>{m.common_cancel()}</Button>{/if}
  </Alert.Description>
</Alert.Root>
