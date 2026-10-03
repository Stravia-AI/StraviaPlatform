<script lang="ts">
import SaveIcon from '@lucide/svelte/icons/save'
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { admin } from '$lib/admin-client'
import { inputValue } from '$lib/utils'
import { loadRpm, saveRpm, rpmQueryKey, type RpmConfig } from '$lib/rpm'
import RequestFailure from './request-failure.svelte'
import { Button } from './ui/button'
import * as Field from './ui/field'
import { Input } from './ui/input'
import { Spinner } from './ui/spinner'

let { poolsOnly = false }: { poolsOnly?: boolean } = $props()
const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
const providers = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list, enabled: poolsOnly }))
let draft = $state<RpmConfig>()
let baseline = $state<RpmConfig>()
let saving = $state(false)
let error = $state('')
let poolName = $state('')
const config = $derived(draft ?? query.data)
function edit(change: (value: RpmConfig) => void) {
  if (!config) return
  baseline ??= structuredClone($state.snapshot(config))
  const next = structuredClone($state.snapshot(config))
  change(next)
  draft = next
}
function limit(value: string): number | null {
  return value === '' ? null : Number(value)
}
function addPool() {
  if (!poolName.trim()) return
  edit((value) => value.pools.push({ id: crypto.randomUUID(), name: poolName.trim(), rpm_limit: null }))
  poolName = ''
}
function members(id: string) {
  return (config?.destinations ?? [])
    .filter((destination) => destination.rpm_pool_id === id)
    .map((destination) => {
      const provider = providers.data?.find((item) => item.id === destination.provider_id)
      const model = destination.model
      return {
        key: JSON.stringify([destination.provider_id, destination.model]),
        providerId: destination.provider_id,
        label: `${provider?.name ?? destination.provider_id} / ${model}`,
      }
    })
}
async function save() {
  if (!draft) return
  saving = true
  error = ''
  try {
    // 保存前读取其他表面的最新配置，只提交本表面拥有的字段。
    const latest = await loadRpm()
    const next = { ...latest }
    if (poolsOnly) {
      const changed = draft.pools.filter(
        (pool) =>
          !baseline?.pools.some(
            (old) => old.id === pool.id && old.name === pool.name && old.rpm_limit === pool.rpm_limit,
          ),
      )
      next.pools = latest.pools.map((pool) => changed.find((updated) => updated.id === pool.id) ?? pool)
      next.pools.push(...changed.filter((pool) => !latest.pools.some((old) => old.id === pool.id)))
    } else {
      next.preferred_wait_ms = draft.preferred_wait_ms
      next.total_wait_ms = draft.total_wait_ms
      next.queue_capacity = draft.queue_capacity
    }
    await saveRpm(next)
    client.setQueryData(rpmQueryKey, next)
    draft = undefined
    baseline = undefined
    toast.success(m.rpm_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<section class="route-section min-w-0" aria-labelledby={poolsOnly ? 'rpm-pools-title' : 'rpm-wait-title'}>
  <h2 id={poolsOnly ? 'rpm-pools-title' : 'rpm-wait-title'} class="route-section-title">
    {poolsOnly ? m.rpm_pools_title() : m.rpm_wait_title()}
  </h2>
  <p class="route-section-description mb-4">{poolsOnly ? m.rpm_pools_help() : m.rpm_wait_help()}</p>
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
          {#if poolsOnly}
            {#each config.pools as pool (pool.id)}
              <div class="min-w-0 flex flex-col gap-3 border-t pt-4">
                <Field.Group class="grid gap-3 sm:grid-cols-2">
                  <Field.Field size="name"
                    ><Field.Label for={`rpm-pool-name-${pool.id}`}>{m.rpm_pool_name()}</Field.Label><Input
                      id={`rpm-pool-name-${pool.id}`}
                      required
                      value={pool.name}
                      oninput={(event: Event) =>
                        edit((value) => {
                          const item = value.pools.find((item) => item.id === pool.id)
                          if (item) item.name = inputValue(event)
                        })} /></Field.Field>
                  <Field.Field size="number"
                    ><Field.Label for={`rpm-pool-limit-${pool.id}`}>RPM</Field.Label><Input
                      id={`rpm-pool-limit-${pool.id}`}
                      type="number"
                      min="1"
                      step="1"
                      placeholder={m.api_key_editor_unlimited()}
                      value={pool.rpm_limit ?? ''}
                      oninput={(event: Event) =>
                        edit((value) => {
                          const item = value.pools.find((item) => item.id === pool.id)
                          if (item) item.rpm_limit = limit(inputValue(event))
                        })} /></Field.Field>
                </Field.Group>
                <p class="text-sm text-muted-foreground">{m.rpm_members()}</p>
                {#if members(pool.id).length}<ul class="flex flex-col gap-1 text-sm break-words">
                    {#each members(pool.id) as member (member.key)}<li>
                        <a
                          class="underline-offset-4 hover:underline"
                          href={resolve(`/providers/${encodeURIComponent(member.providerId)}?view=models`)}
                          >{member.label}</a>
                      </li>{/each}
                  </ul>{:else}<p class="text-sm text-muted-foreground">{m.rpm_no_members()}</p>{/if}
              </div>
            {/each}
            <Field.Field size="fill" class={config.pools.length ? 'border-t pt-4' : undefined}>
              <Field.Label for="rpm-new-pool">{m.rpm_new_pool_name()}</Field.Label>
              <div class="flex min-w-0 gap-2">
                <Input id="rpm-new-pool" class="min-w-0 flex-1" bind:value={poolName} />
                <Button type="button" variant="outline" disabled={!poolName.trim()} onclick={addPool}
                  >{m.rpm_add_pool()}</Button>
              </div>
            </Field.Field>
          {:else}
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
          {/if}
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
