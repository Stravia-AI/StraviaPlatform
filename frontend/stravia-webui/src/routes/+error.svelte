<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { page } from '$app/state'

import { Button } from '#lib/components/ui/button/index.js'

const status = $derived(page.status || 500)
const reason = $derived(
  page.error?.reason === 'module_load'
    ? m.error_reason_module_load()
    : page.error?.reason === 'network'
      ? m.error_reason_network()
      : undefined,
)

function retry(): void {
  window.location.reload()
}
</script>

<svelte:head><title>{m.error_page_unavailable()} · Stravia</title></svelte:head>

<main class="grid place-items-center p-4 sm:p-8">
  <section class="flex w-full max-w-2xl min-w-0 flex-col gap-4" aria-labelledby="error-title">
    <div class="flex items-baseline gap-3">
      <span
        class="shrink-0 rounded-md border border-destructive/40 px-2 py-0.5 font-mono text-sm font-semibold text-destructive tabular-nums"
        >{status}</span>
      <h1 id="error-title" class="font-structural text-2xl font-semibold text-balance">
        {m.error_page_temporarily_unavailable()}
      </h1>
    </div>
    <div class="flex flex-col gap-2 text-sm leading-6" role="alert">
      {#if reason}<p>{reason}</p>{/if}
      <p class="max-w-xl text-muted-foreground">
        {m.error_reload_page_if_problem_continues_check_stravia_running()}
      </p>
    </div>
    <div class="flex flex-wrap gap-2">
      <Button onclick={retry}>{m.error_reload_page()}</Button>
      <Button variant="outline" href="/">{m.common_go_overview()}</Button>
    </div>
  </section>
</main>
