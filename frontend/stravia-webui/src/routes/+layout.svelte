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
import { getAuthState, restoreAuthentication } from '$lib/auth'
import {
  createDesktopUpdateBridge,
  ProductUpdateCoordinator,
  setProductUpdateCoordinator,
} from '$lib/product-update.svelte'

let { children } = $props()
let authReady = $state(false)
const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1, staleTime: 10_000 } },
})
const updates = new ProductUpdateCoordinator(admin.updates, browser && isTauri ? createDesktopUpdateBridge() : null)
setProductUpdateCoordinator(updates)

if (browser) localeState.restore()

onMount(() => {
  let disconnected = false
  let disconnectDesktop: (() => void) | undefined

  const initializeAuth = async () => {
    try {
      const state = await restoreAuthentication(await getAuthState())
      const path = window.location.pathname
      const authenticationPage = path === '/login'
      const setupPage = path === '/setup'

      if (state.mode === 'setup' && !setupPage) {
        window.location.replace('/setup')
        return false
      }
      if ((state.mode === 'server' || state.mode === 'unavailable') && !state.authenticated && !authenticationPage) {
        window.location.replace('/login')
        return false
      }
      if (
        (state.mode === 'desktop' ||
          ((state.mode === 'server' || state.mode === 'unavailable') && state.authenticated)) &&
        (authenticationPage || setupPage)
      ) {
        window.location.replace('/')
        return false
      }
    } catch {
      if (window.location.pathname !== '/login') {
        window.location.replace('/login')
        return false
      }
    }
    authReady = true
    return true
  }

  const initializeUpdates = async () => {
    if (!(await initializeAuth())) return
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
    {#if authReady}
      <ProductUpdateOverlay />
      <AppShell>
        {@render children()}
      </AppShell>
    {/if}
  </Tooltip.Provider>
</QueryClientProvider>
