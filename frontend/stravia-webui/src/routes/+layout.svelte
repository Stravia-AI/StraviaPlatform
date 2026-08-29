<script lang="ts">
import { browser } from '$app/environment'
import { QueryClient, QueryClientProvider } from '@tanstack/svelte-query'
import { onMount } from 'svelte'
import { ModeWatcher } from 'mode-watcher'

import '../app.css'
import AppShell from '$lib/components/app-shell.svelte'
import { Toaster } from '$lib/components/ui/sonner'
import * as Tooltip from '$lib/components/ui/tooltip'
import { localeState } from '$lib/localization.svelte'

let { children } = $props()
const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1, staleTime: 10_000 } },
})

if (browser) localeState.restore()

onMount(() => {
  if (import.meta.env.MODE === 'desktop-e2e') void import('@wdio/tauri-plugin')
})
</script>

<ModeWatcher defaultMode="system" modeStorageKey="stravia-theme" />
<QueryClientProvider client={queryClient}>
  <Tooltip.Provider>
    <Toaster />
    <AppShell>
      {@render children()}
    </AppShell>
  </Tooltip.Provider>
</QueryClientProvider>
