<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'
import ChartNoAxesCombinedIcon from '@lucide/svelte/icons/chart-no-axes-combined'
import CogIcon from '@lucide/svelte/icons/cog'
import GaugeIcon from '@lucide/svelte/icons/gauge'
import KeyRoundIcon from '@lucide/svelte/icons/key-round'
import ImagesIcon from '@lucide/svelte/icons/images'
import LayoutDashboardIcon from '@lucide/svelte/icons/layout-dashboard'
import ListTreeIcon from '@lucide/svelte/icons/list-tree'
import PanelLeftIcon from '@lucide/svelte/icons/panel-left'
import PlugZapIcon from '@lucide/svelte/icons/plug-zap'
import RadioTowerIcon from '@lucide/svelte/icons/radio-tower'
import ScrollTextIcon from '@lucide/svelte/icons/scroll-text'
import SearchCheckIcon from '@lucide/svelte/icons/search-check'
import SignOutIcon from '@lucide/svelte/icons/log-out'
import { userPrefersMode } from 'mode-watcher'
import { onMount } from 'svelte'
import type { Snippet } from 'svelte'

import { admin, isTauri } from '$lib/admin-client'
import { clearAdminToken } from '$lib/auth'
import { createWindowChrome } from '$lib/window-chrome'
import BrandMark from '$lib/components/brand-mark.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import WindowControls from '$lib/components/window-controls.svelte'
import { Button } from '$lib/components/ui/button'
import * as Sheet from '$lib/components/ui/sheet'
import * as Tooltip from '$lib/components/ui/tooltip'

let { children }: { children: Snippet } = $props()

const shellMode = $derived(
  page.url.pathname === '/login' || page.error != null || page.route.id === '/[...path]'
    ? ('titlebar-only' as const)
    : ('navigation' as const),
)

const SIDEBAR_STORAGE_KEY = 'stravia-sidebar-state'
const windowChrome = createWindowChrome()
const materialAvailable = windowChrome.material !== 'opaque'

let navigationOpen = $state(false)
let navigationPanel = $state<HTMLElement | null>(null)
let navigationTrigger = $state<HTMLButtonElement | null>(null)
let isDesktopNavigation = $state(false)
let sidebarCollapsed = $state(false)
let isWindowMaximized = $state(false)
let translucentChrome = $state(false)

const hasNavigation = $derived(shellMode === 'navigation')
const statusQuery = createQuery(() => ({
  queryKey: ['gateway-status'],
  queryFn: admin.settings.status,
  enabled: hasNavigation,
  refetchInterval: 10_000,
}))
const gatewayRunning = $derived(statusQuery.data?.status === 'running')
const gatewayTone = $derived(
  gatewayRunning ? ('healthy' as const) : statusQuery.isError ? ('error' as const) : ('neutral' as const),
)
const currentPath = $derived(page.url.pathname)
const breadcrumbProviderId = $derived(page.route.id === '/providers/[id]' ? (page.params.id ?? '') : '')
const navigationTriggerLabel = $derived(
  isDesktopNavigation
    ? sidebarCollapsed
      ? m.app_shell_expand_navigation()
      : m.app_shell_collapse_navigation()
    : m.app_shell_open_navigation(),
)

$effect(() => {
  const themePreference = userPrefersMode.current
  windowChrome.syncTheme(themePreference === 'system' ? null : themePreference)
})

const navigationGroups = [
  {
    label: m.app_shell_nav_setup,
    items: [
      { href: '/', label: m.app_shell_nav_overview, icon: LayoutDashboardIcon },
      { href: '/providers', label: m.app_shell_nav_model_services, icon: PlugZapIcon },
      { href: '/models', label: m.app_shell_nav_models, icon: ListTreeIcon },
      { href: '/api-keys', label: m.app_shell_nav_api_keys, icon: KeyRoundIcon },
      { href: '/connect', label: m.app_shell_nav_connect_apps, icon: RadioTowerIcon },
    ],
  },
  {
    label: m.app_shell_nav_advanced_features,
    items: [
      { href: '/media-understanding', label: m.app_shell_nav_media_understanding, icon: ImagesIcon },
      { href: '/web-search', label: m.app_shell_nav_web_search, icon: SearchCheckIcon },
    ],
  },
  {
    label: m.app_shell_nav_monitor,
    items: [
      { href: '/logs', label: m.app_shell_nav_request_history, icon: ScrollTextIcon },
      { href: '/stats', label: m.app_shell_nav_usage, icon: ChartNoAxesCombinedIcon },
      { href: '/allowances', label: m.app_shell_nav_allowances, icon: GaugeIcon },
    ],
  },
  { label: m.app_shell_nav_system, items: [{ href: '/settings', label: m.app_shell_nav_settings, icon: CogIcon }] },
] as const

type NavigationItem = (typeof navigationGroups)[number]['items'][number]
type BreadcrumbItem = { label: string; href?: NavigationItem['href'] }

const breadcrumbProvidersQuery = createQuery(() => ({
  queryKey: ['providers'],
  queryFn: admin.providers.list,
  enabled: Boolean(breadcrumbProviderId),
}))
const breadcrumbItems = $derived.by((): BreadcrumbItem[] => {
  const navigationItem = findNavigationItem(currentPath)
  if (!navigationItem) return []

  const parent = { label: navigationItem.label() }
  if (currentPath === navigationItem.href) return [parent]

  if (breadcrumbProviderId) {
    const providerName =
      breadcrumbProvidersQuery.data?.find((provider) => provider.id === breadcrumbProviderId)?.name ??
      m.common_model_service_details()
    return [{ ...parent, href: navigationItem.href }, { label: providerName }]
  }
  if (page.route.id === '/models/[id]') {
    return [{ ...parent, href: navigationItem.href }, { label: m.common_edit() }]
  }
  if (page.route.id === '/api-keys/[id]') {
    return [{ ...parent, href: navigationItem.href }, { label: m.common_edit() }]
  }
  if (page.route.id === '/models/new') {
    return [{ ...parent, href: navigationItem.href }, { label: m.app_shell_create() }]
  }

  return [parent]
})
function findNavigationItem(pathname: string): NavigationItem | undefined {
  for (const group of navigationGroups) {
    for (const item of group.items) {
      if (item.href === '/' ? pathname === '/' : pathname === item.href || pathname.startsWith(`${item.href}/`)) {
        return item
      }
    }
  }
  return undefined
}

function gatewayLabel(compact: boolean): string {
  if (gatewayRunning) return compact ? m.app_shell_running() : m.common_stravia_running()
  return compact ? m.common_unavailable() : m.app_shell_stravia_unavailable()
}

function isCurrent(href: string): boolean {
  return href === '/' ? currentPath === '/' : currentPath === href || currentPath.startsWith(`${href}/`)
}

function signOut(): void {
  clearAdminToken()
  window.location.assign(resolve('/login'))
}

function setSidebarCollapsed(collapsed: boolean): void {
  sidebarCollapsed = collapsed
  localStorage.setItem(SIDEBAR_STORAGE_KEY, collapsed ? 'collapsed' : 'expanded')
}

function handleNavigationTrigger(): void {
  if (isDesktopNavigation) setSidebarCollapsed(!sidebarCollapsed)
  else navigationOpen = true
}

function handleGlobalKeydown(event: KeyboardEvent): void {
  if (
    !hasNavigation ||
    !isDesktopNavigation ||
    event.defaultPrevented ||
    event.altKey ||
    event.shiftKey ||
    (!event.ctrlKey && !event.metaKey) ||
    event.key.toLowerCase() !== 'b'
  ) {
    return
  }

  const target = event.target
  if (
    target instanceof Element &&
    target.closest("input,textarea,select,[contenteditable='true'],[data-no-shortcut]")
  ) {
    return
  }

  event.preventDefault()
  setSidebarCollapsed(!sidebarCollapsed)
}

function focusNavigation(event: Event): void {
  event.preventDefault()
  queueMicrotask(() => navigationPanel?.querySelector<HTMLAnchorElement>('a')?.focus())
}

function restoreNavigationFocus(event: Event): void {
  event.preventDefault()
  navigationTrigger?.focus()
}

onMount(() => {
  const media = window.matchMedia('(min-width: 768px)')
  const syncNavigationMode = () => {
    isDesktopNavigation = media.matches
    if (media.matches) navigationOpen = false
  }
  syncNavigationMode()
  media.addEventListener('change', syncNavigationMode)

  sidebarCollapsed = localStorage.getItem(SIDEBAR_STORAGE_KEY) === 'collapsed'

  translucentChrome = materialAvailable
  document.documentElement.classList.toggle('window-material', translucentChrome)

  let disposed = false
  let stopWindowObservation: (() => void) | undefined
  if (windowChrome.controls === 'custom') {
    void windowChrome
      .observeMaximized((maximized) => (isWindowMaximized = maximized))
      .then((stop) => {
        if (disposed) stop()
        else stopWindowObservation = stop
      })
  }

  return () => {
    disposed = true
    media.removeEventListener('change', syncNavigationMode)
    document.documentElement.classList.remove('window-material')
    stopWindowObservation?.()
  }
})
</script>

<svelte:window onkeydown={handleGlobalKeydown} />

{#snippet navigationItem(item: NavigationItem, closeAfterSelection: boolean, compact: boolean)}
  {@const label = item.label()}
  {@const current = isCurrent(item.href)}
  <Tooltip.Root disabled={!compact}>
    <Tooltip.Trigger>
      {#snippet child({ props })}
        <a
          {...props}
          href={resolve(item.href)}
          aria-label={compact ? label : undefined}
          aria-current={current ? 'page' : undefined}
          class={[
            'relative flex min-h-10 items-center rounded-md transition-[background-color,color] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)]',
            compact ? 'justify-center' : 'gap-3 px-3 text-[0.8125rem] font-medium',
            current
              ? 'bg-sidebar-accent text-sidebar-accent-foreground'
              : 'text-muted-foreground hover:bg-sidebar-accent hover:text-sidebar-accent-foreground',
          ]}
          onclick={() => {
            if (closeAfterSelection) navigationOpen = false
          }}>
          <item.icon class="size-4 shrink-0" />
          {#if !compact}<span class="truncate">{label}</span>{/if}
        </a>
      {/snippet}
    </Tooltip.Trigger>
    {#if compact}<Tooltip.Content side="right" sideOffset={8}>{label}</Tooltip.Content>{/if}
  </Tooltip.Root>
{/snippet}

{#snippet navigation(closeAfterSelection: boolean, compact: boolean)}
  <nav
    class={[
      'flex min-h-0 flex-1 flex-col overflow-y-auto py-4',
      compact ? 'navigation-scrollbar-compact gap-3 px-1' : 'navigation-scrollbar gap-5 px-3',
    ]}
    aria-label={m.app_shell_primary_navigation()}>
    {#each navigationGroups as group (group.label)}
      <div class="flex flex-col gap-1">
        {#if !compact}
          <p
            class="font-structural px-3 pb-1 text-[0.68rem] font-semibold tracking-[0.14em] text-muted-foreground uppercase">
            {group.label()}
          </p>
        {/if}
        {#each group.items as item (item.href)}
          {@render navigationItem(item, closeAfterSelection, compact)}
        {/each}
      </div>
    {/each}
  </nav>
{/snippet}

<div
  class={[
    'shell-root flex h-svh min-h-screen flex-col overflow-hidden text-foreground',
    translucentChrome ? 'bg-transparent' : 'bg-background',
  ]}>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <header
    class={[
      'relative z-40 flex h-10 shrink-0 select-none items-stretch',
      translucentChrome ? 'bg-sidebar/50' : 'bg-sidebar',
    ]}
    onmousedown={(event) => windowChrome.startDrag(event)}
    ondblclick={(event) => windowChrome.toggleMaximize(event)}>
    <div
      class={[
        'flex min-w-0 items-center gap-1 px-1 transition-[width] duration-200 ease-[cubic-bezier(0.2,0,0,1)]',
        hasNavigation ? (sidebarCollapsed ? 'md:w-12' : 'md:w-64') : 'w-auto',
        windowChrome.controls === 'native' ? (sidebarCollapsed && hasNavigation ? 'md:w-32 md:ps-20' : 'ps-20') : '',
      ]}>
      {#if !hasNavigation || !isDesktopNavigation || !sidebarCollapsed}
        <div class="flex min-w-0 items-center gap-2 pe-2" aria-label="Stravia 观策行">
          <BrandMark class="size-6" />
          <span class="font-structural truncate text-sm font-semibold tracking-[0.04em]">STRAVIA</span>
        </div>
      {/if}
      {#if hasNavigation}
        <Button
          bind:ref={navigationTrigger}
          variant="ghost"
          class="relative me-2 ms-auto size-6 shrink-0 rounded-md text-sidebar-foreground before:absolute before:-inset-2 before:content-[''] hover:bg-sidebar-accent hover:text-sidebar-accent-foreground aria-expanded:bg-transparent aria-expanded:text-sidebar-foreground aria-expanded:hover:bg-sidebar-accent aria-expanded:hover:text-sidebar-accent-foreground"
          size="icon-sm"
          onclick={handleNavigationTrigger}
          aria-expanded={isDesktopNavigation ? !sidebarCollapsed : navigationOpen}
          aria-label={navigationTriggerLabel}
          title={navigationTriggerLabel}>
          <PanelLeftIcon />
        </Button>
      {/if}
    </div>

    <div class="flex min-w-0 flex-1 items-center px-4">
      {#if hasNavigation && breadcrumbItems.length > 0}
        <nav class="min-w-0 text-xs text-muted-foreground" aria-label={m.app_shell_breadcrumb()}>
          <ol class="flex min-w-0 items-center gap-2">
            {#each breadcrumbItems as item, index (`${item.href ?? 'current'}-${item.label}`)}
              <li class="flex min-w-0 items-center gap-2">
                {#if index > 0}<span aria-hidden="true">/</span>{/if}
                {#if item.href}
                  <a
                    class="inline-flex min-h-10 min-w-0 items-center truncate font-medium transition-colors hover:text-foreground"
                    href={resolve(item.href)}>{item.label}</a>
                {:else}
                  <span class="truncate font-medium text-foreground" aria-current="page">{item.label}</span>
                {/if}
              </li>
            {/each}
          </ol>
        </nav>
      {/if}
    </div>
    {#if windowChrome.controls === 'custom'}
      <WindowControls
        isMaximized={isWindowMaximized}
        minimizeLabel={m.app_shell_minimize_window()}
        maximizeLabel={m.app_shell_maximize_window()}
        restoreLabel={m.app_shell_restore_window()}
        closeLabel={m.app_shell_close_window()}
        onMinimize={windowChrome.minimize}
        onToggleMaximize={() => windowChrome.toggleMaximize()}
        onClose={windowChrome.close} />
    {/if}
  </header>

  {#if hasNavigation}
    <div class={['flex min-h-0 flex-1', translucentChrome ? 'bg-sidebar/50' : 'bg-sidebar']}>
      <aside
        class={[
          'hidden min-h-0 shrink-0 flex-col overflow-hidden text-sidebar-foreground transition-[width] duration-200 ease-[cubic-bezier(0.2,0,0,1)] md:flex',
          sidebarCollapsed ? 'w-12' : 'w-64',
          translucentChrome ? 'bg-transparent' : 'bg-sidebar',
        ]}>
        {@render navigation(false, sidebarCollapsed)}

        <div class={['border-t border-sidebar-border p-1', sidebarCollapsed ? 'flex flex-col items-center' : 'p-3']}>
          <StatusIndicator
            class={sidebarCollapsed ? 'justify-center [&>span:last-child]:sr-only' : ''}
            compact
            label={gatewayLabel(sidebarCollapsed)}
            tone={gatewayTone} />
          {#if !isTauri}
            <Button
              class={sidebarCollapsed ? 'mt-1 size-10' : 'mt-2 w-full justify-start'}
              variant="ghost"
              size={sidebarCollapsed ? 'icon-sm' : 'default'}
              onclick={signOut}
              aria-label={m.app_shell_sign_out()}
              title={sidebarCollapsed ? m.app_shell_sign_out() : undefined}>
              <SignOutIcon data-icon={sidebarCollapsed ? undefined : 'inline-start'} />
              {#if !sidebarCollapsed}{m.app_shell_sign_out()}{/if}
            </Button>
          {:else if !sidebarCollapsed}
            <p class="mt-2 px-3 py-2 text-xs text-muted-foreground">
              {m.app_shell_local_desktop_session()}
            </p>
          {/if}
        </div>
      </aside>

      <div class="min-w-0 flex-1 overflow-hidden p-0 md:pr-2 md:pb-2">
        <main
          class="shell-main shell-scrollbar flex h-full min-w-0 flex-col overflow-y-auto bg-background p-4 md:rounded-lg">
          <div class="mx-auto w-full max-w-[1800px]">
            {@render children()}
          </div>
        </main>
      </div>
    </div>
  {:else}
    <div class="grid min-h-0 flex-1 overflow-y-auto bg-background">
      {@render children()}
    </div>
  {/if}
</div>

{#if hasNavigation}
  <Sheet.Root bind:open={navigationOpen}>
    <Sheet.Content
      bind:ref={navigationPanel}
      side="left"
      class="data-[side=left]:w-[min(18rem,100vw)] data-[side=left]:sm:max-w-[18rem] gap-0 p-0"
      onOpenAutoFocus={focusNavigation}
      onCloseAutoFocus={restoreNavigationFocus}
      closeLabel={m.app_shell_close_navigation()}>
      <Sheet.Header class="border-b">
        <div class="flex items-center gap-2">
          <BrandMark class="size-8" />
          <div>
            <Sheet.Title class="font-structural tracking-[0.04em]">STRAVIA</Sheet.Title>
            <Sheet.Description>{m.app_shell_local_ai_gateway()}</Sheet.Description>
          </div>
        </div>
      </Sheet.Header>
      {@render navigation(true, false)}
      <Sheet.Footer class="border-t">
        <StatusIndicator compact label={gatewayLabel(false)} tone={gatewayTone} />
        {#if !isTauri}
          <Button class="w-full justify-start" variant="ghost" onclick={signOut}>
            <SignOutIcon data-icon="inline-start" />
            {m.app_shell_sign_out()}
          </Button>
        {:else}
          <p class="text-xs text-muted-foreground">{m.app_shell_local_desktop_session()}</p>
        {/if}
      </Sheet.Footer>
    </Sheet.Content>
  </Sheet.Root>
{/if}

<style>
.shell-root {
  /* Keep inner route overflow from enlarging the document scroll area; main owns navigation-page scrolling. */
  contain: size layout;
}

.shell-scrollbar {
  scrollbar-gutter: stable both-edges;
}

.navigation-scrollbar {
  scrollbar-gutter: stable both-edges;
}

.navigation-scrollbar-compact {
  scrollbar-width: none;
}

.navigation-scrollbar-compact::-webkit-scrollbar {
  display: none;
}
</style>
