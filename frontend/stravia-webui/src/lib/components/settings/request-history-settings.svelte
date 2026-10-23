<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import InlineSettingInput from '#lib/components/inline-setting-input.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const queryClient = useQueryClient()
const retentionQuery = createQuery(() => ({
  queryKey: ['setting', 'log_retention_days'],
  queryFn: () => admin.settings.get('log_retention_days'),
}))

const retention = $derived(retentionQuery.data === undefined ? undefined : (retentionQuery.data || '7').trim())

function validate(draft: string): string | undefined {
  const value = Number(draft.trim())
  return Number.isInteger(value) && value >= 1 && value <= 365
    ? undefined
    : m.settings_retention_must_whole_number_between_1_365()
}

async function save(next: string): Promise<string> {
  const value = String(Number.parseInt(next, 10))
  await admin.settings.set('log_retention_days', value)
  queryClient.setQueryData(['setting', 'log_retention_days'], value)
  return value
}
</script>

<SettingsSection id="logs" title={m.common_request_history()}>
  {#if retentionQuery.error}
    <RequestFailure
      title={m.settings_some_settings_not_loaded()}
      message={localizeBackendErrorMessage(retentionQuery.error)}
      retry={() => void retentionQuery.refetch()}
      retrying={retentionQuery.isFetching} />
  {:else if retention !== undefined}
    <SettingRow
      title={m.settings_retention_period_days()}
      labelFor="log-retention"
      description={m.settings_automatically_deletes_older_request_history()}
      controlSize="number">
      {#snippet children({ titleId, descriptionId })}
        <InlineSettingInput
          id="log-retention"
          type="number"
          min={1}
          max={365}
          labelledby={titleId}
          describedby={descriptionId}
          value={retention}
          {validate}
          onSave={save} />
      {/snippet}
    </SettingRow>
  {:else}
    <Skeleton class="my-3 h-9" aria-busy="true" />
  {/if}
</SettingsSection>
