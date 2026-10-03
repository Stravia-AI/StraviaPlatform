<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '$lib/backend-error'
import { loadRpm, rpmQueryKey, saveRpm } from '$lib/rpm'
import { inputValue } from '$lib/utils'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Popover from '$lib/components/ui/popover'
import { Spinner } from '$lib/components/ui/spinner'

// 移动端行没有表头，showLabel 让摘要自带「RPM」前缀。
let { providerId, modelId, showLabel = false }: { providerId: string; modelId: string; showLabel?: boolean } = $props()

const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
const destination = $derived(
  query.data?.destinations.find((item) => item.provider_id === providerId && item.model === modelId),
)
const summary = $derived(
  destination?.rpm_limit ? m.rpm_value({ limit: destination.rpm_limit }) : m.api_key_editor_unlimited(),
)
let open = $state(false)
let limit = $state('')
let saving = $state(false)
let error = $state('')

function openChange(next: boolean) {
  if (next) {
    limit = destination?.rpm_limit ? String(destination.rpm_limit) : ''
    error = ''
  }
  open = next
}

async function save() {
  saving = true
  error = ''
  try {
    // 合并最新配置，只替换本模型的目的地条目，避免覆盖其他表面刚保存的修改。
    const latest = await loadRpm()
    const destinations = latest.destinations.filter(
      (item) => !(item.provider_id === providerId && item.model === modelId),
    )
    const rpmLimit = limit !== '' ? Number(limit) : null
    if (rpmLimit !== null) destinations.push({ provider_id: providerId, model: modelId, rpm_limit: rpmLimit })
    const next = { ...latest, destinations }
    await saveRpm(next)
    client.setQueryData(rpmQueryKey, next)
    toast.success(m.rpm_saved())
    open = false
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<Popover.Root bind:open={() => open, openChange}>
  <Popover.Trigger>
    {#snippet child({ props })}
      <Button
        {...props}
        variant="ghost"
        class="h-auto min-h-10 max-w-full justify-start px-2 text-left font-normal"
        aria-label={m.rpm_edit_label({ model: modelId, value: summary })}
        disabled={!query.data}>
        {#if query.isPending}<Spinner />{:else if query.error}{m.common_unavailable()}{:else}<span class="truncate"
            >{#if showLabel}<span class="text-muted-foreground">RPM · </span>{/if}{summary}</span
          >{/if}
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="flex w-80 max-w-[calc(100vw-2rem)] flex-col p-0" role="dialog">
    <Popover.Header class="border-b border-border/60 px-4 py-3">
      <Popover.Title>{m.rpm_column()}</Popover.Title>
      <Popover.Description class="break-all font-technical text-xs">{modelId}</Popover.Description>
    </Popover.Header>
    <form
      class="flex flex-col"
      onsubmit={(event) => {
        event.preventDefault()
        void save()
      }}>
      <fieldset disabled={saving} class="flex min-w-0 flex-col gap-4 p-4">
        <p class="text-sm text-muted-foreground">{m.rpm_limit_help()}</p>
        <Field.Field orientation="vertical">
          <Field.Label for="rpm-limit-value">{m.rpm_requests_per_minute()}</Field.Label>
          <Input
            id="rpm-limit-value"
            type="number"
            min="1"
            step="1"
            placeholder={m.api_key_editor_unlimited()}
            value={limit}
            oninput={(event: Event) => (limit = inputValue(event))} />
        </Field.Field>
        {#if error}<p role="alert" class="text-sm text-destructive">{error}</p>{/if}
      </fieldset>
      <div class="flex justify-end gap-2 border-t border-border/60 px-4 py-3">
        <Button type="button" variant="outline" onclick={() => (open = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving} aria-busy={saving}
          >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.rpm_save_limit()}</Button>
      </div>
    </form>
  </Popover.Content>
</Popover.Root>
