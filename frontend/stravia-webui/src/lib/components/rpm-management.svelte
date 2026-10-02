<script lang="ts">
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
import { Checkbox } from './ui/checkbox'
import * as Field from './ui/field'
import { Input } from './ui/input'
import { Spinner } from './ui/spinner'

let { providerId, poolsOnly = false }: { providerId?: string; poolsOnly?: boolean } = $props()
const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
const routes = createQuery(() => ({
  queryKey: ['models'],
  queryFn: admin.models.list,
  enabled: Boolean(providerId) || poolsOnly,
}))
let draft = $state<RpmConfig>()
let baseline = $state<RpmConfig>()
let saving = $state(false)
let error = $state('')
let model = $state('')
let providerOnly = $state(false)
let poolName = $state('')
const config = $derived(draft ?? query.data)
const destinations = $derived(config?.destinations.filter((item) => item.provider_id === providerId) ?? [])
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
function addDestination() {
  if (!providerId || (!providerOnly && !model.trim())) return
  const upstream = providerOnly ? null : model.trim()
  if (destinations.some((item) => item.model === upstream)) return
  edit((value) => value.destinations.push({ provider_id: providerId, model: upstream, rpm_limit: null }))
  model = ''
}
function addPool() {
  if (!poolName.trim()) return
  edit((value) => value.pools.push({ id: crypto.randomUUID(), name: poolName.trim(), rpm_limit: null }))
  poolName = ''
}
function members(id: string) {
  return (routes.data ?? []).flatMap((route) =>
    route.targets
      .filter((target) => target.rpm_pool_id === id)
      .map((target) => ({
        id: target.id,
        label: `${route.model_id} → ${target.provider_id} / ${target.model ?? m.rpm_provider_only()}`,
      })),
  )
}
function destinationMembers(upstream: string | null) {
  return (routes.data ?? []).flatMap((route) =>
    route.targets
      .filter((target) => !target.rpm_pool_id && target.provider_id === providerId && target.model === upstream)
      .map((target) => ({ id: target.id, label: route.model_id })),
  )
}
async function save() {
  if (!draft) return
  saving = true
  error = ''
  try {
    // 保存前读取其他表面的最新配置，只提交本表面拥有的字段。
    const latest = await loadRpm()
    const next = { ...latest }
    if (providerId) {
      const changed = draft.destinations.filter(
        (item) =>
          item.provider_id === providerId &&
          !baseline?.destinations.some(
            (old) =>
              old.provider_id === item.provider_id && old.model === item.model && old.rpm_limit === item.rpm_limit,
          ),
      )
      next.destinations = latest.destinations.map(
        (item) =>
          changed.find((updated) => updated.provider_id === item.provider_id && updated.model === item.model) ?? item,
      )
      next.destinations.push(
        ...changed.filter(
          (item) =>
            !latest.destinations.some((old) => old.provider_id === item.provider_id && old.model === item.model),
        ),
      )
    } else if (poolsOnly) {
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

<section
  class="route-section min-w-0"
  aria-labelledby={poolsOnly ? 'rpm-pools-title' : providerId ? 'rpm-destinations-title' : 'rpm-wait-title'}>
  <h2
    id={poolsOnly ? 'rpm-pools-title' : providerId ? 'rpm-destinations-title' : 'rpm-wait-title'}
    class="route-section-title">
    {poolsOnly ? m.rpm_pools_title() : providerId ? m.rpm_upstream_title() : m.rpm_wait_title()}
  </h2>
  <p class="route-section-description mb-4">{providerId || poolsOnly ? m.rpm_upstream_help() : m.rpm_wait_help()}</p>
  {#if query.error}
    <RequestFailure message={localizeBackendErrorMessage(query.error)} retry={() => query.refetch()} />
  {:else if config}
    <form
      class="flex flex-col gap-4"
      onsubmit={(event) => {
        event.preventDefault()
        void save()
      }}>
      <fieldset disabled={saving} class="min-w-0 flex flex-col gap-4">
        {#if providerId}
          {#each destinations as destination (JSON.stringify([destination.provider_id, destination.model]))}
            <Field.Field size="number">
              <Field.Label for={`rpm-destination-${encodeURIComponent(destination.model ?? '__provider__')}`}
                >{destination.model ?? m.rpm_provider_only()} · RPM</Field.Label>
              <Input
                id={`rpm-destination-${encodeURIComponent(destination.model ?? '__provider__')}`}
                type="number"
                min="1"
                step="1"
                placeholder={m.api_key_editor_unlimited()}
                value={destination.rpm_limit ?? ''}
                oninput={(event: Event) =>
                  edit((value) => {
                    const item = value.destinations.find(
                      (item) => item.provider_id === providerId && item.model === destination.model,
                    )
                    if (item) item.rpm_limit = limit(inputValue(event))
                  })} />
              <Field.Description
                >{m.rpm_effective_quota({
                  limit: destination.rpm_limit ?? m.api_key_editor_unlimited(),
                })}</Field.Description>
              {#if destinationMembers(destination.model).length}
                <ul aria-label={m.rpm_members()} class="flex flex-col gap-1 text-sm break-words">
                  {#each destinationMembers(destination.model) as member (member.id)}<li>{member.label}</li>{/each}
                </ul>
              {/if}
            </Field.Field>
          {/each}
          <div class="flex flex-wrap items-end gap-3">
            <Field.Field class="min-w-0 flex-1" size="fill"
              ><Field.Label for="rpm-upstream-model">{m.rpm_upstream_model()}</Field.Label><Input
                id="rpm-upstream-model"
                bind:value={model}
                disabled={providerOnly} /></Field.Field>
            <Field.Field orientation="horizontal" class="min-h-10 w-auto">
              <Checkbox id="rpm-provider-only" bind:checked={providerOnly} disabled={saving} />
              <Field.Label for="rpm-provider-only">{m.rpm_provider_only()}</Field.Label>
            </Field.Field>
            <Button type="button" variant="outline" onclick={addDestination}>{m.rpm_add_destination()}</Button>
          </div>
          <a class="text-sm underline underline-offset-4" href={resolve('/providers#rpm-pools')}
            >{m.rpm_pools_title()}</a>
        {:else if poolsOnly}
          {#if routes.error}<RequestFailure
              message={localizeBackendErrorMessage(routes.error)}
              retry={() => routes.refetch()} />{/if}
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
              <p class="text-sm">{m.rpm_effective_quota({ limit: pool.rpm_limit ?? m.api_key_editor_unlimited() })}</p>
              <p class="text-sm text-muted-foreground">{m.rpm_members()}</p>
              {#if routes.isPending}<Spinner />{:else if members(pool.id).length}<ul
                  class="flex flex-col gap-1 text-sm break-words">
                  {#each members(pool.id) as member (member.id)}<li>{member.label}</li>{/each}
                </ul>{:else if !routes.error}<p class="text-sm text-muted-foreground">{m.rpm_no_members()}</p>{/if}
            </div>
          {/each}
          <div class="flex flex-wrap items-end gap-3">
            <Field.Field size="name" class="min-w-0 flex-1"
              ><Field.Label for="rpm-new-pool">{m.rpm_pool_name()}</Field.Label><Input
                id="rpm-new-pool"
                bind:value={poolName} /></Field.Field
            ><Button type="button" variant="outline" onclick={addPool}>{m.rpm_add_pool()}</Button>
          </div>
        {:else}
          <Field.FieldGroup>
            <Field.Field size="number"
              ><Field.Label for="rpm-preferred-wait">{m.rpm_preferred_wait()}</Field.Label><Input
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
              ><Field.Label for="rpm-total-wait">{m.rpm_total_wait()}</Field.Label><Input
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
              ><Field.Label for="rpm-queue-capacity">{m.rpm_queue_capacity()}</Field.Label><Input
                id="rpm-queue-capacity"
                type="number"
                min="1"
                step="1"
                value={config.queue_capacity}
                oninput={(event: Event) =>
                  edit((value) => {
                    value.queue_capacity = Number(inputValue(event))
                  })} /></Field.Field>
          </Field.FieldGroup>
        {/if}
      </fieldset>
      {#if draft}<p role="status" class="text-sm text-muted-foreground">{m.common_settings_unsaved()}</p>{/if}
      {#if error}<p role="alert" class="text-sm text-destructive">{error}</p>{/if}
      <Button type="submit" disabled={!draft || saving}
        >{#if saving}<Spinner />{/if}{m.rpm_save()}</Button>
    </form>
  {:else}<Spinner />{/if}
</section>
