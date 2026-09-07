<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import PageHeader from '$lib/components/page-header.svelte'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const settingKey = 'reversible_redaction_enabled'
const queryKey = ['setting', settingKey]
const queryClient = useQueryClient()
const settingQuery = createQuery(() => ({
  queryKey,
  queryFn: () => admin.settings.get(settingKey),
}))
let draft = $state<boolean>()
let saving = $state(false)
let saveError = $state('')
const storedEnabled = $derived(settingQuery.data === 'true')
const enabled = $derived(draft ?? storedEnabled)
const canSave = $derived(settingQuery.isSuccess && !saving && enabled !== storedEnabled)

async function save(): Promise<void> {
  if (!canSave) return
  const value = enabled ? 'true' : 'false'
  saving = true
  saveError = ''
  try {
    await admin.settings.set(settingKey, value)
    queryClient.setQueryData(queryKey, value)
    draft = undefined
    toast.success(m.reversible_redaction_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}
</script>

<svelte:head><title>{m.reversible_redaction_title()} · Stravia</title></svelte:head>

{#snippet pageActions()}
  <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
    {#if saving}<Spinner data-icon="inline-start" />{/if}
    {m.common_save_settings()}
  </Button>
{/snippet}

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.reversible_redaction_title()}
    description={m.reversible_redaction_summary()}
    actions={pageActions} />

  <section class="route-section" aria-labelledby="redaction-setting-title">
    <Field.Group>
      <Field.Field orientation="horizontal" data-disabled={!settingQuery.isSuccess || saving}>
        <Field.Content>
          <Field.Label id="redaction-setting-title" for="reversible-redaction-enabled">
            {m.reversible_redaction_enable()}
          </Field.Label>
          <Field.Description id="redaction-setting-description">
            {m.reversible_redaction_scope()}
          </Field.Description>
        </Field.Content>
        <Switch
          id="reversible-redaction-enabled"
          checked={enabled}
          onCheckedChange={(checked) => { draft = checked }}
          disabled={!settingQuery.isSuccess || saving}
          aria-describedby="redaction-setting-description redaction-off-description" />
      </Field.Field>
    </Field.Group>
    {#if settingQuery.isPending}
      <p class="mt-4 text-sm text-muted-foreground" role="status">{m.reversible_redaction_loading()}</p>
    {:else if settingQuery.isError}
      <p class="mt-4 text-sm text-destructive" role="alert">{m.reversible_redaction_load_failed()}</p>
      <Button class="mt-3" variant="outline" onclick={() => void settingQuery.refetch()}>{m.common_retry()}</Button>
    {/if}
    {#if saveError}
      <p class="mt-4 text-sm text-destructive" role="alert">{saveError}</p>
    {/if}
  </section>

  <section class="route-section" aria-labelledby="redaction-protection-title">
    <div class="route-section-header">
      <h2 id="redaction-protection-title" class="route-section-title">{m.reversible_redaction_protection_title()}</h2>
    </div>
    <div class="flex flex-col gap-3 text-sm text-muted-foreground">
      <p>{m.reversible_redaction_protection()}</p>
      <p>{m.reversible_redaction_rules()}</p>
      <p>{m.reversible_redaction_mapping()}</p>
      <p>{m.reversible_redaction_failures()}</p>
    </div>
  </section>

  <section class="route-section" aria-labelledby="redaction-off-title">
    <div class="route-section-header">
      <h2 id="redaction-off-title" class="route-section-title">{m.reversible_redaction_off_title()}</h2>
    </div>
    <p id="redaction-off-description" class="text-sm text-muted-foreground">{m.reversible_redaction_off()}</p>
  </section>

  <section class="route-section" aria-labelledby="redaction-limits-title">
    <div class="route-section-header">
      <h2 id="redaction-limits-title" class="route-section-title">{m.reversible_redaction_limits_title()}</h2>
    </div>
    <div class="flex flex-col gap-3 text-sm text-muted-foreground">
      <p>{m.reversible_redaction_tools_warning()}</p>
      <p>{m.reversible_redaction_limits()}</p>
    </div>
  </section>
</div>
