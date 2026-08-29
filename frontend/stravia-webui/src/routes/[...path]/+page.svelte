<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'

import { Button } from '$lib/components/ui/button'

const requestedPath = $derived(page.url.pathname)
</script>

<svelte:head><title>{m.not_found_page_not_found()} · Stravia</title></svelte:head>

<main class="grid place-items-center p-4 sm:p-6">
  <section class="w-full max-w-2xl border-y py-10" aria-labelledby="not-found-title">
    <p class="font-structural text-[0.72rem] font-semibold tracking-[0.14em] text-primary uppercase">404 · Stravia</p>
    <h1 id="not-found-title" class="font-structural mt-3 text-[1.625rem] leading-8 font-semibold">
      {m.not_found_page_not_found()}
    </h1>
    <p class="mt-2 text-sm leading-6 text-muted-foreground">
      {m.not_found_page_value_does_not_exist_have_moved({ requestedPath: requestedPath })}
    </p>
    <Button class="mt-5" href="/">{m.common_go_overview()}</Button>
  </section>
</main>
