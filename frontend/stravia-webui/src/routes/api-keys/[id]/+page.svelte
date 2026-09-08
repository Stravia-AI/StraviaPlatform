<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import ApiKeyEditor from '$lib/components/api-key-editor.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

const apiKeyId = $derived(page.params.id ?? '')
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const webSearchQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const mediaUnderstandingQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
const apiKey = $derived(apiKeysQuery.data?.find((item) => item.id === apiKeyId))

async function returnToApiKeys(): Promise<void> {
  await goto(resolve('/api-keys'))
}
</script>

<svelte:head><title>{apiKey?.name ?? m.common_api_key()} · Stravia</title></svelte:head>

{#if apiKeysQuery.isPending || modelsQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if apiKeysQuery.isError || modelsQuery.isError}
  <div class="route-page">
    <PageHeader eyebrow={m.common_setup()} title={m.common_api_key()} />
    <RequestFailure
      title={m.api_keys_api_keys_not_loaded()}
      message={localizeBackendErrorMessage(apiKeysQuery.error ?? modelsQuery.error)}
      retry={() => Promise.all([apiKeysQuery.refetch(), modelsQuery.refetch()])}
      retrying={apiKeysQuery.isFetching || modelsQuery.isFetching} />
    <Button href="/api-keys" variant="outline">{m.api_keys_back_api_keys()}</Button>
  </div>
{:else if !apiKey}
  <div class="route-page">
    <PageHeader eyebrow={m.common_setup()} title={m.api_keys_api_key_not_found()} />
    <Button href="/api-keys" variant="outline">{m.api_keys_back_api_keys()}</Button>
  </div>
{:else}
  <ApiKeyEditor
    presentation="page"
    {apiKey}
    models={modelsQuery.data ?? []}
    webSearchEnabled={webSearchQuery.data?.enabled ?? false}
    mediaUnderstandingEnabled={mediaUnderstandingQuery.data?.enabled ?? false}
    onSaved={returnToApiKeys} />
{/if}
