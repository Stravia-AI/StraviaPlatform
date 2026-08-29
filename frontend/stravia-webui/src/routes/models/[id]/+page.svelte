<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import ModelEditor from '$lib/components/model-editor.svelte'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

const routeId = $derived(page.params.id ?? '')
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const model = $derived(modelsQuery.data?.find((item) => item.id === routeId))
</script>

<svelte:head><title>{model?.name ?? m.common_model()} · Stravia</title></svelte:head>

{#if providersQuery.isPending || modelsQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if providersQuery.isError || modelsQuery.isError}
  <p class="text-sm text-destructive">
    {localizeBackendErrorMessage(providersQuery.error ?? modelsQuery.error)}
  </p>
{:else if !model}
  <div class="route-page">
    <h1 class="text-2xl font-semibold">{m.models_model_not_found()}</h1>
    <Button href="/models" variant="outline">{m.models_back_models()}</Button>
  </div>
{:else}
  <ModelEditor {model} providers={providersQuery.data ?? []} />
{/if}
