<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '$lib/backend-error'
import { loadRpm, rpmQueryKey, saveRpm } from '$lib/rpm'
import { inputValue } from '$lib/utils'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Popover from '$lib/components/ui/popover'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'

// 移动端行没有表头，showLabel 让摘要自带「RPM」前缀。
let { providerId, modelId, showLabel = false }: { providerId: string; modelId: string; showLabel?: boolean } = $props()

const ownLimit = '__own__'
const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
const destination = $derived(
  query.data?.destinations.find((item) => item.provider_id === providerId && item.model === modelId),
)
const pool = $derived(query.data?.pools.find((item) => item.id === destination?.rpm_pool_id))
const summary = $derived(
  pool
    ? m.rpm_pool_value({ name: pool.name })
    : destination?.rpm_limit
      ? m.rpm_value({ limit: destination.rpm_limit })
      : m.api_key_editor_unlimited(),
)
let open = $state(false)
let mode = $state(ownLimit)
let limit = $state('')
let saving = $state(false)
let error = $state('')

function openChange(next: boolean) {
  if (next) {
    mode = destination?.rpm_pool_id ?? ownLimit
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
    const rpmLimit = mode === ownLimit && limit !== '' ? Number(limit) : null
    const rpmPoolId = mode === ownLimit ? null : mode
    if (rpmLimit !== null || rpmPoolId !== null)
      destinations.push({ provider_id: providerId, model: modelId, rpm_limit: rpmLimit, rpm_pool_id: rpmPoolId })
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
          <Field.Label for="rpm-limit-source">{m.rpm_count_against()}</Field.Label>
          <Select.Root type="single" bind:value={mode}>
            <Select.Trigger id="rpm-limit-source" class="w-full">
              {mode === ownLimit
                ? m.rpm_own_limit()
                : (query.data?.pools.find((item) => item.id === mode)?.name ?? mode)}
            </Select.Trigger>
            <Select.Content>
              <Select.Item value={ownLimit}>{m.rpm_own_limit()}</Select.Item>
              {#if query.data?.pools.length}
                <Select.Group>
                  <Select.GroupHeading>{m.rpm_pools_title()}</Select.GroupHeading>
                  {#each query.data.pools as item (item.id)}
                    <Select.Item value={item.id}
                      >{item.name} · {item.rpm_limit
                        ? m.rpm_value({ limit: item.rpm_limit })
                        : m.api_key_editor_unlimited()}</Select.Item>
                  {/each}
                </Select.Group>
              {/if}
            </Select.Content>
          </Select.Root>
          <a class="text-sm underline underline-offset-4" href={resolve('/providers#rpm-pools')}
            >{m.rpm_manage_pools()}</a>
        </Field.Field>
        {#if mode === ownLimit}
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
        {/if}
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
