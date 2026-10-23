<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { createQuery } from '@tanstack/svelte-query'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import MediaUnderstandingSection from '#lib/components/media-understanding-section.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const configQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
</script>

<svelte:head><title>{m.media_understanding_title()} · Stravia</title></svelte:head>

<div class="route-page">
  <PageHeader title={m.media_understanding_title()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.media_understanding_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {:else if configQuery.isPending}
    <div class="content-column flex flex-col gap-4" aria-label={m.common_settings_loading()} aria-busy="true">
      <Skeleton class="h-12" />
      <Skeleton class="h-12" />
    </div>
  {:else if configQuery.data}
    <div class="content-column flex flex-col gap-8">
      <MediaUnderstandingSection kind="image" config={configQuery.data.image} />
      <MediaUnderstandingSection kind="audio" config={configQuery.data.audio} />
    </div>
  {/if}
</div>
