<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { page } from '$app/state'

import { Button } from '#lib/components/ui/button/index.js'

const requestedPath = $derived(page.url.pathname)
</script>

<svelte:head><title>{m.not_found_page_not_found()} · Stravia</title></svelte:head>

<main class="grid place-items-center p-4 sm:p-8">
  <section class="flex w-full max-w-2xl min-w-0 flex-col gap-4" aria-labelledby="not-found-title">
    <div class="flex items-baseline gap-3">
      <span
        class="shrink-0 rounded-md border px-2 py-0.5 font-mono text-sm font-semibold text-muted-foreground tabular-nums"
        >404</span>
      <h1 id="not-found-title" class="font-structural text-2xl font-semibold text-balance">
        {m.not_found_page_not_found()}
      </h1>
    </div>
    <p class="text-sm leading-6 [overflow-wrap:anywhere] text-muted-foreground">
      {m.not_found_page_value_does_not_exist_have_moved({ requestedPath: requestedPath })}
    </p>
    <Button class="self-start" href="/">{m.common_go_overview()}</Button>
  </section>
</main>
