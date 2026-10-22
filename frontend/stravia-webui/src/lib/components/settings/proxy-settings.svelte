<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin, type ProxySettings } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import InlineSettingInput from '#lib/components/inline-setting-input.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const queryClient = useQueryClient()
const proxyQuery = createQuery(() => ({ queryKey: ['outbound-proxy-settings'], queryFn: admin.settings.proxy }))

let editorOpen = $state(false)
let urlDraft = $state('')
let saving = $state(false)
let error = $state('')

const proxy = $derived(proxyQuery.data)

async function persist(patch: Partial<ProxySettings>): Promise<ProxySettings> {
  if (!proxy) throw new Error(m.settings_some_settings_not_loaded())
  const value = { ...proxy, ...patch }
  await admin.settings.saveProxy(value)
  queryClient.setQueryData(['outbound-proxy-settings'], value)
  return value
}

function openEditor(): void {
  urlDraft = proxy?.url ?? ''
  error = ''
  editorOpen = true
}

async function saveUrl(): Promise<void> {
  saving = true
  error = ''
  try {
    await persist({ url: urlDraft.trim() })
    editorOpen = false
    toast.success(m.settings_proxy_settings_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}

async function saveBypass(next: string): Promise<string> {
  return (await persist({ bypass: next.trim() })).bypass
}
</script>

<SettingsSection id="proxy" title={m.settings_proxy()}>
  {#if proxyQuery.error}
    <RequestFailure
      title={m.settings_some_settings_not_loaded()}
      message={localizeBackendErrorMessage(proxyQuery.error)}
      retry={() => void proxyQuery.refetch()}
      retrying={proxyQuery.isFetching} />
  {:else if proxy}
    <SettingRow
      title={m.settings_proxy_url()}
      value={proxy.url || m.settings_proxy_not_configured()}
      onedit={openEditor} />
    <SettingRow title={m.settings_bypass_hosts()} labelFor="proxy-bypass" controlSize="fill">
      {#snippet children({ titleId })}
        <InlineSettingInput
          id="proxy-bypass"
          labelledby={titleId}
          technical
          value={proxy.bypass}
          placeholder="localhost,127.0.0.1,.internal"
          onSave={saveBypass} />
      {/snippet}
    </SettingRow>
  {:else if proxyQuery.isPending}
    <div class="flex flex-col gap-3 py-3" aria-busy="true"><Skeleton class="h-9" /><Skeleton class="h-9" /></div>
  {/if}
</SettingsSection>

<SettingsSheet
  bind:open={editorOpen}
  title={m.settings_edit_proxy()}
  draft={() => urlDraft}
  submitLabel={m.settings_save_proxy()}
  {saving}
  {error}
  onsubmit={saveUrl}>
  <Field.Field size="fill">
    <Field.FieldLabel for="proxy-url">{m.settings_proxy_url()}</Field.FieldLabel>
    <Input
      id="proxy-url"
      class="font-technical"
      bind:value={urlDraft}
      disabled={saving}
      placeholder="http://127.0.0.1:7890" />
  </Field.Field>
</SettingsSheet>
