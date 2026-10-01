<script lang="ts">
import BrandMark from '$lib/components/brand-mark.svelte'
import BrandWordmark from '$lib/components/brand-wordmark.svelte'
import StartupProgress from '$lib/components/startup-progress.svelte'
import { Button } from '$lib/components/ui/button'
import * as m from '$lib/paraglide/messages.js'
import type { ServerStartupState } from '$lib/server-startup'

let { state, connectionError = false }: { state: ServerStartupState; connectionError?: boolean } = $props()
const failed = $derived(state.status === 'failed' || connectionError)
</script>

<main class="flex min-h-svh min-w-0 flex-col bg-background p-4 text-foreground sm:p-8" data-testid="server-startup">
  <div class="flex items-center gap-2" aria-label="Stravia">
    <BrandMark class="size-6" />
    <BrandWordmark class="text-sm" />
  </div>
  <section class="m-auto flex w-full min-w-0 max-w-xl flex-col gap-6 py-8" aria-labelledby="server-startup-title">
    <div role={failed ? 'alert' : undefined}>
      <h1 id="server-startup-title" class="font-structural text-2xl font-semibold text-balance">
        {connectionError
          ? m.server_startup_connection_failed()
          : state.status === 'failed'
            ? m.server_startup_failed_title()
            : m.desktop_startup_starting_title()}
      </h1>
      <p class="mt-2 text-sm text-muted-foreground text-pretty">
        {connectionError
          ? m.server_startup_connection_action()
          : state.status === 'failed'
            ? m.server_startup_failed_action()
            : m.server_startup_description()}
      </p>
    </div>
    {#if failed}
      <Button onclick={() => window.location.reload()}>{m.server_startup_refresh()}</Button>
    {:else}
      <StartupProgress
        progress={state.progress ?? { phase: 'gateway', label: 'Preparing services', completed: 0, total: null }} />
    {/if}
  </section>
</main>
