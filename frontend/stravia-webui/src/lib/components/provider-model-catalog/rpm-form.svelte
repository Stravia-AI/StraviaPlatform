<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { loadRpm, rpmQueryKey, saveRpm } from '#lib/rpm.js'
import { cn, inputValue } from '#lib/utils.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

let {
  providerId,
  modelId,
  embedded = false,
  onSaved,
  onCancel,
  onDirtyChange,
}: {
  providerId: string
  modelId: string
  // 嵌入模型详情时由页面底部的「保存模型」统一提交，不再渲染自己的按钮、内边距和分隔线。
  embedded?: boolean
  onSaved?: () => void
  onCancel?: () => void
  onDirtyChange?: (dirty: boolean) => void
} = $props()

const id = $props.id()
const client = useQueryClient()
const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
let draft = $state({ providerId: '', modelId: '', limit: '', dirty: false, initialized: false })
let saving = $state(false)
let error = $state('')
const ready = $derived(draft.initialized && draft.providerId === providerId && draft.modelId === modelId)

$effect(() => {
  const provider = providerId
  const model = modelId
  const data = query.data
  untrack(() => {
    if (draft.providerId !== provider || draft.modelId !== model) {
      draft = { providerId: provider, modelId: model, limit: '', dirty: false, initialized: false }
      error = ''
      onDirtyChange?.(false)
    }
    // 缓存刷新只同步未编辑表单，不能覆盖用户正在输入的草稿。
    if (data && !draft.dirty) {
      const destination = data.destinations.find((item) => item.provider_id === provider && item.model === model)
      draft.limit = destination?.rpm_limit ? String(destination.rpm_limit) : ''
      draft.initialized = true
    }
  })
})

export async function save(): Promise<void> {
  if (saving || !ready || query.error) return
  const provider = providerId
  const model = modelId
  const submitted = draft
  const rpmLimit = submitted.limit !== '' ? Number(submitted.limit) : null
  saving = true
  error = ''
  try {
    // 合并最新配置，仅替换本目的地，保留其他目的地及等待／队列设置。
    const latest = await loadRpm()
    const destinations = latest.destinations.filter((item) => !(item.provider_id === provider && item.model === model))
    if (rpmLimit !== null) destinations.push({ provider_id: provider, model, rpm_limit: rpmLimit })
    const next = { ...latest, destinations }
    await saveRpm(next)
    if (draft === submitted) {
      draft.dirty = false
      onDirtyChange?.(false)
    }
    client.setQueryData(rpmQueryKey, next)
    toast.success(m.rpm_saved())
    if (providerId === provider && modelId === model) onSaved?.()
  } catch (cause) {
    if (providerId === provider && modelId === model) error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<form
  class={cn('flex flex-col', embedded && 'gap-3')}
  onsubmit={(event) => {
    event.preventDefault()
    void save()
  }}>
  <fieldset disabled={saving || !ready} class={cn('flex min-w-0 flex-col gap-4', !embedded && 'p-4')}>
    <Field.Group>
      <Field.Field size="number" data-invalid={!!error}>
        <Field.Label for={`${id}-limit`}>{m.rpm_requests_per_minute()}</Field.Label>
        <Input
          id={`${id}-limit`}
          type="number"
          min="1"
          step="1"
          placeholder={ready ? m.api_key_editor_unlimited() : undefined}
          value={ready ? draft.limit : ''}
          aria-describedby={`${id}-help`}
          aria-invalid={!!error}
          oninput={(event: Event) => {
            draft.limit = inputValue(event)
            draft.dirty = true
            onDirtyChange?.(true)
          }} />
        <Field.Description id={`${id}-help`}>{m.rpm_limit_help()}</Field.Description>
        {#if error}<Field.Error>{error}</Field.Error>{/if}
      </Field.Field>
    </Field.Group>
  </fieldset>
  {#if query.error}
    <div class={cn('flex flex-col items-start gap-2', !embedded && 'px-4 pb-4')}>
      <p role="alert" class="text-sm text-destructive">{localizeBackendErrorMessage(query.error)}</p>
      <Button type="button" variant="outline" disabled={query.isFetching} onclick={() => void query.refetch()}>
        {#if query.isFetching}<Spinner data-icon="inline-start" />{/if}{m.common_retry()}
      </Button>
    </div>
  {:else if !ready}
    <p role="status" class={cn('flex items-center gap-2 text-sm text-muted-foreground', !embedded && 'px-4 pb-4')}>
      <Spinner />{m.common_settings_loading()}
    </p>
  {/if}
  {#if !embedded}
    <div class="flex justify-end gap-2 border-t border-border/60 px-4 py-3">
      {#if onCancel}<Button type="button" variant="outline" disabled={saving} onclick={onCancel}
          >{m.common_cancel()}</Button
        >{/if}
      <Button type="submit" disabled={saving || !ready || !!query.error} aria-busy={saving}>
        {#if saving}<Spinner data-icon="inline-start" />{/if}{m.rpm_save_limit()}
      </Button>
    </div>
  {/if}
</form>
