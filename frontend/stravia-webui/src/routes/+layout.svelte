<script lang="ts">
import { browser } from '$app/environment'
import { QueryClient, QueryClientProvider } from '@tanstack/svelte-query'
import { onMount } from 'svelte'
import { ModeWatcher } from 'mode-watcher'

import '../app.css'
import AppShell from '$lib/components/app-shell.svelte'
import ProductUpdateOverlay from '$lib/components/product-update-overlay.svelte'
import { Toaster } from '$lib/components/ui/sonner'
import * as Tooltip from '$lib/components/ui/tooltip'
import { localeState } from '$lib/localization.svelte'
import { admin, isTauri } from '$lib/admin-client'
import {
  createDesktopUpdateBridge,
  ProductUpdateCoordinator,
  setProductUpdateCoordinator,
} from '$lib/product-update.svelte'

let { children } = $props()
const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1, staleTime: 10_000 } },
})
const updates = new ProductUpdateCoordinator(
  admin.updates,
  browser && isTauri ? createDesktopUpdateBridge() : null,
)
setProductUpdateCoordinator(updates)

if (browser) localeState.restore()

onMount(() => {
  let disconnected = false
  let disconnectDesktop: (() => void) | undefined

  const initializeUpdates = async () => {
    if (import.meta.env.MODE === 'desktop-e2e') {
      await import('@wdio/tauri-plugin')
      await updates.load()
    } else {
      await updates.automaticCheck()
    }

    const disconnect = await updates.connectDesktopBridge()
    if (disconnected) disconnect()
    else disconnectDesktop = disconnect
  }
  void initializeUpdates()

  return () => {
    disconnected = true
    disconnectDesktop?.()
  }
})
</script>

<ModeWatcher defaultMode="system" modeStorageKey="stravia-theme" />
<QueryClientProvider client={queryClient}>
  <Tooltip.Provider>
    <Toaster />
    <ProductUpdateOverlay />
    <AppShell>
      {@render children()}
    </AppShell>
  </Tooltip.Provider>
</QueryClientProvider>
