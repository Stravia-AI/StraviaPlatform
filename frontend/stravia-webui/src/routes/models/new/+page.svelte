<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import ModelEditor from '$lib/components/model-editor.svelte'
import { Spinner } from '$lib/components/ui/spinner'

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const initialProviderId = $derived(page.url.searchParams.get('provider') ?? '')
const initialModelId = $derived(page.url.searchParams.get('model') ?? '')
</script>

<svelte:head><title>{m.common_add_model()} · Stravia</title></svelte:head>

{#if providersQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if providersQuery.isError}
  <p class="text-sm text-destructive">
    {localizeBackendErrorMessage(providersQuery.error)}
  </p>
{:else}
  <ModelEditor providers={providersQuery.data ?? []} {initialProviderId} {initialModelId} />
{/if}
