<script lang="ts">
import SaveIcon from '@lucide/svelte/icons/save'
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { inputValue } from '$lib/utils'
import { loadRpm, saveRpm, rpmQueryKey, type RpmConfig } from '$lib/rpm'
import RequestFailure from './request-failure.svelte'
import { Button } from './ui/button'
import * as Field from './ui/field'
import { Input } from './ui/input'
import { Spinner } from './ui/spinner'

const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
let draft = $state<RpmConfig>()
let saving = $state(false)
let error = $state('')
const config = $derived(draft ?? query.data)
function edit(change: (value: RpmConfig) => void) {
  if (!config) return
  const next = structuredClone($state.snapshot(config))
  change(next)
  draft = next
}
async function save() {
  if (!draft) return
  saving = true
  error = ''
  try {
    // 保存前读取其他表面的最新配置，只提交本表面拥有的字段。
    const latest = await loadRpm()
    const next = { ...latest }
    next.preferred_wait_ms = draft.preferred_wait_ms
    next.total_wait_ms = draft.total_wait_ms
    next.queue_capacity = draft.queue_capacity
    await saveRpm(next)
    client.setQueryData(rpmQueryKey, next)
    draft = undefined
    toast.success(m.rpm_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<section class="route-section min-w-0" aria-labelledby="rpm-wait-title">
  <h2 id="rpm-wait-title" class="route-section-title">
    {m.rpm_wait_title()}
  </h2>
  <p class="route-section-description mb-4">{m.rpm_wait_help()}</p>
  {#if query.error}
    <RequestFailure message={localizeBackendErrorMessage(query.error)} retry={() => query.refetch()} />
  {:else if config}
    <form
      onsubmit={(event) => {
        event.preventDefault()
        void save()
      }}>
      <Field.FieldGroup>
        <fieldset disabled={saving} class="min-w-0 flex flex-col gap-5">
          <Field.Field size="number"
            ><Field.Label for="rpm-preferred-wait" hint={m.rpm_default_value({ value: 5000 })}
              >{m.rpm_preferred_wait()}</Field.Label
            ><Input
              id="rpm-preferred-wait"
              type="number"
              min="0"
              step="1"
              value={config.preferred_wait_ms}
              oninput={(event: Event) =>
                edit((value) => {
                  value.preferred_wait_ms = Number(inputValue(event))
                })} /></Field.Field>
          <Field.Field size="number"
            ><Field.Label for="rpm-total-wait" hint={m.rpm_default_value({ value: 30000 })}
              >{m.rpm_total_wait()}</Field.Label
            ><Input
              id="rpm-total-wait"
              type="number"
              min="0"
              step="1"
              value={config.total_wait_ms}
              oninput={(event: Event) =>
                edit((value) => {
                  value.total_wait_ms = Number(inputValue(event))
                })} /></Field.Field>
          <Field.Field size="number"
            ><Field.Label for="rpm-queue-capacity" hint={m.rpm_default_value({ value: 128 })}
              >{m.rpm_queue_capacity()}</Field.Label
            ><Input
              id="rpm-queue-capacity"
              type="number"
              min="1"
              step="1"
              value={config.queue_capacity}
              oninput={(event: Event) =>
                edit((value) => {
                  value.queue_capacity = Number(inputValue(event))
                })} /></Field.Field>
        </fieldset>
        {#if error}<p role="alert" class="text-sm text-destructive">{error}</p>{/if}
        <div class="field-actions items-center">
          {#if draft}<p role="status" class="text-sm text-muted-foreground">{m.common_settings_unsaved()}</p>{/if}
          <Button type="submit" disabled={!draft || saving} aria-busy={saving}
            >{#if saving}<Spinner data-icon="inline-start" />{:else}<SaveIcon
                data-icon="inline-start" />{/if}{m.rpm_save()}</Button>
        </div>
      </Field.FieldGroup>
    </form>
  {:else}<Spinner />{/if}
</section>
