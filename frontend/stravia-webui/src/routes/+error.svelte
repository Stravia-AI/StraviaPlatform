<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { page } from '$app/state'

import { Button } from '$lib/components/ui/button'

const status = $derived(page.status || 500)

function retry(): void {
  window.location.reload()
}
</script>

<svelte:head><title>{m.error_page_unavailable()} · Stravia</title></svelte:head>

<main class="grid place-items-center p-4 sm:p-6">
  <section class="route-page w-full max-w-2xl justify-center">
    <div class="border-y py-10">
      <p class="font-structural text-xs font-semibold tracking-[0.14em] text-destructive uppercase">
        {m.error_error()} · {status}
      </p>
      <h1 class="mt-3 text-2xl font-semibold tracking-tight text-balance">
        {m.error_page_temporarily_unavailable()}
      </h1>
      <p class="mt-2 max-w-xl text-sm leading-6 text-muted-foreground">
        {m.error_reload_page_if_problem_continues_check_stravia_running()}
      </p>
      <div class="mt-5 flex flex-wrap gap-2">
        <Button onclick={retry}>{m.error_reload_page()}</Button>
        <Button variant="outline" href="/">{m.common_go_overview()}</Button>
      </div>
    </div>
  </section>
</main>
