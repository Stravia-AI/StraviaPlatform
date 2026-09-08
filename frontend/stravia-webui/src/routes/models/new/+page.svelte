<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import ModelEditor from '$lib/components/model-editor.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const initialProviderId = $derived(page.url.searchParams.get('provider') ?? '')
const initialModelId = $derived(page.url.searchParams.get('model') ?? '')
</script>

<svelte:head><title>{m.common_add_model()} · Stravia</title></svelte:head>

{#if providersQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if providersQuery.isError}
  <div class="route-page">
    <PageHeader eyebrow={m.common_setup()} title={m.common_add_model()} />
    <RequestFailure
      title={m.providers_model_services_not_loaded()}
      message={localizeBackendErrorMessage(providersQuery.error)}
      retry={() => providersQuery.refetch()}
      retrying={providersQuery.isFetching} />
    <Button href="/models" variant="outline">{m.models_back_models()}</Button>
  </div>
{:else}
  <ModelEditor providers={providersQuery.data ?? []} {initialProviderId} {initialModelId} />
{/if}
