<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { capabilityStatus } from '#lib/capability-status.js'
import ApiKeyEditor from '#lib/components/api-key-editor.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

const apiKeyId = $derived(page.params.id ?? '')
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))

const webSearchQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const mediaUnderstandingQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
const mediaGenerationQuery = createQuery(() => ({
  queryKey: ['media-generation-config'],
  queryFn: admin.mediaGeneration.config.get,
}))

const webSearchStatus = $derived(capabilityStatus(webSearchQuery, (config) => config.enabled))
const mediaUnderstandingStatus = $derived(
  capabilityStatus(mediaUnderstandingQuery, (config) => config.image.enabled || config.audio.enabled),
)
const mediaGenerationStatus = $derived(capabilityStatus(mediaGenerationQuery, (config) => config.config.enabled))

const apiKey = $derived(apiKeysQuery.data?.find((item) => item.id === apiKeyId))

async function returnToApiKeys(): Promise<void> {
  await goto(resolve('api-keys'))
}
</script>

<svelte:head><title>{apiKey ? apiKey.name.trim() || apiKey.id : m.common_api_key()} · Stravia</title></svelte:head>

{#if apiKeysQuery.isPending || modelsQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if apiKeysQuery.isError || modelsQuery.isError}
  <div class="route-page">
    <PageHeader title={m.common_api_key()} />
    <RequestFailure
      title={m.api_keys_api_keys_not_loaded()}
      message={localizeBackendErrorMessage(apiKeysQuery.error ?? modelsQuery.error)}
      retry={() => Promise.all([apiKeysQuery.refetch(), modelsQuery.refetch()])}
      retrying={apiKeysQuery.isFetching || modelsQuery.isFetching} />
    <Button href="/api-keys" variant="outline">{m.api_keys_back_api_keys()}</Button>
  </div>
{:else if !apiKey}
  <div class="route-page">
    <PageHeader title={m.api_keys_api_key_not_found()} />
    <Button href="/api-keys" variant="outline">{m.api_keys_back_api_keys()}</Button>
  </div>
{:else}
  <ApiKeyEditor
    presentation="page"
    {apiKey}
    models={modelsQuery.data ?? []}
    {webSearchStatus}
    {mediaUnderstandingStatus}
    {mediaGenerationStatus}
    onSaved={returnToApiKeys} />
{/if}
