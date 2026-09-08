<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { effectiveModelDisplayName } from '$lib/logical-model'
import ModelEditor from '$lib/components/model-editor.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

const routeId = $derived(page.params.id ?? '')
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const routeQuery = createQuery(() => ({
  queryKey: ['models', routeId],
  queryFn: () => admin.models.get(routeId),
  enabled: Boolean(routeId),
}))
const model = $derived(routeQuery.data)
</script>

<svelte:head><title>{model ? effectiveModelDisplayName(model) : m.common_model()} · Stravia</title></svelte:head>

{#if providersQuery.isPending || routeQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if providersQuery.isError || routeQuery.isError}
  <div class="route-page">
    <PageHeader eyebrow={m.common_setup()} title={m.common_model()} />
    <RequestFailure
      title={m.models_models_not_loaded()}
      message={localizeBackendErrorMessage(providersQuery.error ?? routeQuery.error)}
      retry={() => Promise.all([providersQuery.refetch(), routeQuery.refetch()])}
      retrying={providersQuery.isFetching || routeQuery.isFetching} />
    <Button href="/models" variant="outline">{m.models_back_models()}</Button>
  </div>
{:else if !model}
  <div class="route-page">
    <PageHeader eyebrow={m.common_setup()} title={m.models_model_not_found()} />
    <Button href="/models" variant="outline">{m.models_back_models()}</Button>
  </div>
{:else}
  <ModelEditor {model} providers={providersQuery.data ?? []} />
{/if}
