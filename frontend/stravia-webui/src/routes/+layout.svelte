<script lang="ts">
import { browser } from '$app/environment'
import { afterNavigate } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { QueryClient, QueryClientProvider } from '@tanstack/svelte-query'
import { ModeWatcher } from 'mode-watcher'
import { onMount } from 'svelte'
import type { Snippet } from 'svelte'

import '../app.css'
import AppShell from '$lib/components/app-shell.svelte'
import DesktopStartup from '$lib/components/desktop-startup.svelte'
import DesktopStartupNotices from '$lib/components/desktop-startup-notices.svelte'
import ProductUpdateOverlay from '$lib/components/product-update-overlay.svelte'
import { Button } from '$lib/components/ui/button'
import { Toaster } from '$lib/components/ui/sonner'
import * as Tooltip from '$lib/components/ui/tooltip'
import { setConnectSetup } from '$lib/connect-setup'
import type { ConnectSetup } from '$lib/connect-setup'
import { connectDesktopStartup, desktopFrontendFailure, initialDesktopStartupState } from '$lib/desktop-startup'
import type { DesktopStartupState } from '$lib/desktop-startup'
import { localeState } from '$lib/localization.svelte'
import * as m from '$lib/paraglide/messages.js'
import { admin, isTauri } from '$lib/admin-client'
import { getAuthState, restoreAuthentication } from '$lib/auth'
import {
  createDesktopUpdateBridge,
  ProductUpdateCoordinator,
  setProductUpdateCoordinator,
} from '$lib/product-update.svelte'

let { children }: { children: Snippet } = $props()
const connectSetup = $state<ConnectSetup>({ draft: undefined, createKey: false })
setConnectSetup(connectSetup)
const isSetupResource = $derived(/^\/(providers|models|api-keys)(\/|$)/.test(page.url.pathname))
afterNavigate(() => {
  if (!isSetupResource && page.url.pathname !== resolve('/connect')) {
    connectSetup.draft = undefined
    connectSetup.createKey = false
  }
})

let authReady = $state(false)
let desktopStartup = $state.raw<DesktopStartupState>(initialDesktopStartupState)
const desktopSurfaceState = $derived<DesktopStartupState>(
  desktopStartup.status === 'ready' && !authReady
    ? { ...desktopStartup, status: 'starting', stage: 'session' }
    : desktopStartup,
)
const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1, staleTime: 10_000 } },
})
const updates = new ProductUpdateCoordinator(admin.updates, browser && isTauri ? createDesktopUpdateBridge() : null)
setProductUpdateCoordinator(updates)

// The WDIO plugin installs a desktop automation bridge as a module side effect, so desktop-e2e must load it before startup/auth work.
const desktopE2eBridge =
  browser && isTauri && import.meta.env.MODE === 'desktop-e2e' ? import('@wdio/tauri-plugin') : Promise.resolve()

if (browser) localeState.restore()

onMount(() => {
  let disposed = false
  let applicationInitializationStarted = false
  let disconnectDesktopStartup: (() => void) | undefined
  let disconnectDesktopUpdates: (() => void) | undefined

  const failDesktopStartup = (stage: 'session' | 'desktop', details: string) => {
    if (disposed) return
    authReady = false
    desktopStartup = { ...desktopFrontendFailure(stage, details), logPath: desktopStartup.logPath }
  }

  const initializeAuth = async (desktop: boolean) => {
    try {
      const state = await restoreAuthentication(await getAuthState())
      const path = window.location.pathname
      const authenticationPage = path === '/login'
      const setupPage = path === '/setup'

      if (desktop && (state.mode !== 'desktop' || !state.authenticated)) {
        failDesktopStartup('session', 'frontend_auth_state_unavailable')
        return false
      }
      if (!desktop && state.mode === 'setup' && !setupPage) {
        window.location.replace('/setup')
        return false
      }
      if (!desktop && (state.mode === 'server' || state.mode === 'unavailable') && !state.authenticated) {
        if (!authenticationPage) {
          window.location.replace('/login')
          return false
        }
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
      if (desktop) {
        failDesktopStartup('session', 'frontend_auth_bootstrap_failed')
        return false
      }
      if (window.location.pathname !== '/login') {
        window.location.replace('/login')
        return false
      }
    }
    return true
  }

  const initializeApplication = async (desktop: boolean) => {
    if (applicationInitializationStarted || disposed) return
    applicationInitializationStarted = true

    if (!(await initializeAuth(desktop)) || disposed) return
    if (desktop && desktopStartup.status !== 'ready') return
    authReady = true

    try {
      if (import.meta.env.MODE === 'desktop-e2e') await updates.load()
      else await updates.automaticCheck()

      const disconnect = await updates.connectDesktopBridge()
      if (disposed) disconnect()
      else disconnectDesktopUpdates = disconnect
    } catch {
      // Product update integration is optional and must not hide an otherwise ready gateway.
      console.warn('Stravia product update integration is unavailable')
    }
  }

  const applyDesktopStartup = (state: DesktopStartupState) => {
    if (disposed) return
    desktopStartup = state
    if (state.status === 'ready') {
      localeState.enableDesktopSync()
      void initializeApplication(true)
    } else authReady = false
  }

  const initializeDesktop = async () => {
    try {
      await desktopE2eBridge
    } catch {
      failDesktopStartup('desktop', 'desktop_e2e_bridge_failed')
      return
    }

    try {
      const disconnect = await connectDesktopStartup(applyDesktopStartup)
      if (disposed) disconnect()
      else disconnectDesktopStartup = disconnect
    } catch {
      failDesktopStartup('desktop', 'frontend_startup_bridge_failed')
    }
  }

  if (isTauri) void initializeDesktop()
  else void initializeApplication(false)

  return () => {
    disposed = true
    disconnectDesktopStartup?.()
    disconnectDesktopUpdates?.()
  }
})
</script>

<ModeWatcher defaultMode="system" modeStorageKey="stravia-theme" />
{#if isTauri && (!authReady || desktopStartup.status !== 'ready')}
  <DesktopStartup state={desktopSurfaceState} />
{:else}
  <QueryClientProvider client={queryClient}>
    <Tooltip.Provider>
      <Toaster toastOptions={{ class: 'pointer-events-none' }} />
      {#if authReady}
        <ProductUpdateOverlay />
        <AppShell>
          {#if isTauri}
            <DesktopStartupNotices state={desktopStartup} />
          {/if}
          {#if connectSetup.draft && isSetupResource}
            <div class="mb-6 flex flex-wrap items-center justify-between gap-3 border-b pb-4">
              <p class="text-sm text-muted-foreground">{m.connect_continue_setup_description()}</p>
              <Button
                href={resolve('/connect')}
                variant="outline"
                onclick={() => {
                  void queryClient.invalidateQueries({ queryKey: ['models'] })
                  void queryClient.invalidateQueries({ queryKey: ['api-keys'] })
                }}>{m.connect_continue_setup()}</Button>
            </div>
          {/if}
          {@render children()}
        </AppShell>
      {/if}
    </Tooltip.Provider>
  </QueryClientProvider>
{/if}
