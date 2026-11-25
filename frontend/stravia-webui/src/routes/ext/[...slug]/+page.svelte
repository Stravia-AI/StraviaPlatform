<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'
import type { Component } from 'svelte'
import { untrack } from 'svelte'

import { capabilitiesQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { ensureBuildExtensionsRegistered } from '#lib/extensions/index.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const extensions = ensureBuildExtensionsRegistered()
const capabilitiesQuery = createQuery(() => capabilitiesQueryOptions())

const capabilities = $derived(capabilitiesQuery.data?.capabilities ?? [])
const extPath = $derived(`/${page.params.slug ?? ''}`)
const matched = $derived(capabilitiesQuery.data ? extensions.matchRoute(capabilities, extPath) : null)

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let loadedEntry = $state<{ id: string; component: Component<any> } | null>(null)
let loadError = $state<unknown>(null)
let reloadToken = $state(0)

$effect(() => {
  const current = matched
  void reloadToken
  if (!current) {
    loadedEntry = null
    loadError = null
    return
  }
  const route = current.route
  if (untrack(() => loadedEntry?.id === route.id)) {
    loadError = null
    return
  }
  let cancelled = false
  loadedEntry = null
  loadError = null
  void route
    .load()
    .then((mod) => {
      if (cancelled) return
      loadedEntry = { id: route.id, component: mod.default }
    })
    .catch((error: unknown) => {
      if (cancelled) return
      loadError = error
    })
  return () => {
    cancelled = true
  }
})
</script>

<svelte:head>
  {#if !capabilitiesQuery.isPending && !capabilitiesQuery.isError && !matched}
    <title>{m.not_found_page_not_found()} · Stravia</title>
  {/if}
</svelte:head>

{#if capabilitiesQuery.isPending}
  <div class="route-page flex min-w-0 flex-col gap-6">
    <Skeleton class="h-9 w-56" />
    <Skeleton class="h-24 w-full" />
    <Skeleton class="h-64 w-full" />
  </div>
{:else if capabilitiesQuery.isError}
  <div class="route-page flex min-w-0 flex-col gap-6">
    <RequestFailure
      title={m.not_found_page_not_found()}
      message={localizeBackendErrorMessage(capabilitiesQuery.error)}
      retry={() => capabilitiesQuery.refetch()}
      retrying={capabilitiesQuery.isFetching} />
  </div>
{:else if matched}
  {#if loadError}
    <div class="route-page flex min-w-0 flex-col gap-6">
      <RequestFailure
        title={m.not_found_page_not_found()}
        message={localizeBackendErrorMessage(loadError)}
        retry={() => {
          reloadToken += 1
        }} />
    </div>
  {:else if loadedEntry && loadedEntry.id === matched.route.id}
    {@const LoadedComponent = loadedEntry.component}
    <LoadedComponent params={matched.params} {capabilities} capabilitiesData={capabilitiesQuery.data} />
  {:else}
    <div class="route-page flex min-w-0 flex-col gap-6">
      <Skeleton class="h-9 w-56" />
      <Skeleton class="h-24 w-full" />
      <Skeleton class="h-64 w-full" />
    </div>
  {/if}
{:else}
  <div class="route-page grid min-h-[calc(100svh-8rem)] place-items-center">
    <section class="flex w-full max-w-2xl min-w-0 flex-col gap-4" aria-labelledby="ext-not-found-title">
      <div class="flex items-baseline gap-3">
        <span
          class="shrink-0 rounded-md border px-2 py-0.5 font-mono text-sm font-semibold text-muted-foreground tabular-nums">
          404
        </span>
        <h1 id="ext-not-found-title" class="font-structural text-2xl font-semibold text-balance">
          {m.not_found_page_not_found()}
        </h1>
      </div>
      <p class="text-sm leading-6 [overflow-wrap:anywhere] text-muted-foreground">
        {m.not_found_page_value_does_not_exist_have_moved({ requestedPath: page.url.pathname })}
      </p>
      <Button class="self-start" href={resolve('/')}>{m.common_go_overview()}</Button>
    </section>
  </div>
{/if}
