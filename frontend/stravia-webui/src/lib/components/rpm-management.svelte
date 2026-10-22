<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { loadRpm, saveRpm, rpmQueryKey, type RpmConfig } from '#lib/rpm.js'

const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))

type Draft = Pick<RpmConfig, 'preferred_wait_ms' | 'total_wait_ms' | 'queue_capacity'>

let open = $state(false)
let draft = $state<Draft>({ preferred_wait_ms: 0, total_wait_ms: 0, queue_capacity: 0 })
let saving = $state(false)
let error = $state('')

const config = $derived(query.data)

function openEditor(): void {
  if (!config) return
  draft = {
    preferred_wait_ms: config.preferred_wait_ms,
    total_wait_ms: config.total_wait_ms,
    queue_capacity: config.queue_capacity,
  }
  error = ''
  open = true
}

async function save(): Promise<void> {
  saving = true
  error = ''
  try {
    // 保存前读取其他表面的最新配置，只提交本表面拥有的字段。
    const latest = await loadRpm()
    const next = {
      ...latest,
      preferred_wait_ms: Number(draft.preferred_wait_ms),
      total_wait_ms: Number(draft.total_wait_ms),
      queue_capacity: Number(draft.queue_capacity),
    }
    await saveRpm(next)
    client.setQueryData(rpmQueryKey, next)
    open = false
    toast.success(m.rpm_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<SettingsSection id="rpm" titleId="rpm-wait-title" title={m.rpm_wait_title()}>
  {#snippet action()}
    {#if config}
      <Button type="button" variant="outline" size="sm" onclick={openEditor}>{m.common_edit()}</Button>
    {/if}
  {/snippet}
  {#if query.error}
    <RequestFailure
      message={localizeBackendErrorMessage(query.error)}
      retry={() => void query.refetch()}
      retrying={query.isFetching} />
  {:else if config}
    <SettingRow
      layout="inline"
      title={m.rpm_preferred_wait()}
      value={m.rpm_milliseconds({ value: config.preferred_wait_ms })} />
    <SettingRow
      layout="inline"
      title={m.rpm_total_wait()}
      value={m.rpm_milliseconds({ value: config.total_wait_ms })} />
    <SettingRow layout="inline" title={m.rpm_queue_capacity()} value={String(config.queue_capacity)} />
  {:else}
    <Skeleton class="my-3 h-9" aria-busy="true" />
  {/if}
</SettingsSection>

<SettingsSheet
  bind:open
  title={m.rpm_wait_title()}
  draft={() => draft}
  submitLabel={m.rpm_save()}
  {saving}
  {error}
  onsubmit={save}>
  <Field.FieldGroup>
    <Field.Field size="number">
      <Field.FieldLabel for="rpm-preferred-wait" hint={m.rpm_default_value({ value: 5000 })}>
        {m.rpm_preferred_wait()}
      </Field.FieldLabel>
      <Input
        id="rpm-preferred-wait"
        type="number"
        min="0"
        step="1"
        bind:value={draft.preferred_wait_ms}
        disabled={saving} />
    </Field.Field>
    <Field.Field size="number">
      <Field.FieldLabel for="rpm-total-wait" hint={m.rpm_default_value({ value: 30000 })}>
        {m.rpm_total_wait()}
      </Field.FieldLabel>
      <Input id="rpm-total-wait" type="number" min="0" step="1" bind:value={draft.total_wait_ms} disabled={saving} />
    </Field.Field>
    <Field.Field size="number">
      <Field.FieldLabel for="rpm-queue-capacity" hint={m.rpm_default_value({ value: 128 })}>
        {m.rpm_queue_capacity()}
      </Field.FieldLabel>
      <Input
        id="rpm-queue-capacity"
        type="number"
        min="1"
        step="1"
        bind:value={draft.queue_capacity}
        disabled={saving} />
    </Field.Field>
  </Field.FieldGroup>
</SettingsSheet>
