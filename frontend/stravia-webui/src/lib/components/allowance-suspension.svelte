<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import type { AllowanceSuspension } from '$lib/types/provider-allowance'
import { formatLogTime } from '$lib/format'
import { localeState } from '$lib/localization.svelte'
import * as Alert from '$lib/components/ui/alert'
let { suspension }: { suspension: AllowanceSuspension } = $props()
</script>

<Alert.Root variant="warning" role="status" data-testid="allowance-suspension">
  <Alert.Title>{m.allowances_suspended()}</Alert.Title>
  <Alert.Description class="grid gap-1">
    <p>{m.allowances_suspended_since({ time: formatLogTime(suspension.suspended_at, localeState.current) })}</p>
    <p class="break-all">{m.allowances_suspension_triggers({ keys: suspension.triggered_keys.join(', ') })}</p>
    {#if suspension.earliest_reset_at != null}
      <p>{m.allowances_reset_at({ time: formatLogTime(suspension.earliest_reset_at, localeState.current) })}</p>
    {/if}
    <p>{m.allowances_suspension_recovery()}</p>
  </Alert.Description>
</Alert.Root>
