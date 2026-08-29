<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery } from '@tanstack/svelte-query'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'

import { getDesktopPortState } from '$lib/desktop-port'
import type { PortOwner } from '$lib/desktop-port'
import { buttonVariants } from '$lib/components/ui/button'

const portQuery = createQuery(() => ({
  queryKey: ['desktop-port-state'],
  queryFn: getDesktopPortState,
  refetchInterval: (query) => (query.state.data?.ownerLookup === 'identifying' ? 500 : false),
}))

const state = $derived(portQuery.data)

function ownersLabel(owners: PortOwner[]): string {
  return owners.map((owner) => `${owner.name} (PID ${owner.pid})`).join(', ')
}
</script>

{#if state?.mode === 'configError'}
  <section class="border-l-2 border-destructive bg-destructive/5 px-4 py-3" aria-labelledby="desktop-port-error-title">
    <div class="flex gap-3">
      <CircleAlertIcon class="mt-0.5 size-4 shrink-0 text-destructive" />
      <div class="min-w-0 flex-1">
        <h2 id="desktop-port-error-title" class="text-sm font-semibold">
          {m.desktop_port_notice_desktop_port_setting_unavailable()}
        </h2>
        <p class="mt-1 text-sm text-muted-foreground">
          {m.desktop_port_notice_fallback_reason()}
        </p>
        <p class="font-technical mt-1 text-xs text-muted-foreground">{state.configError}</p>
        <a
          class={buttonVariants({ variant: 'outline', size: 'sm', class: 'mt-3' })}
          href={resolve('/settings#desktop')}>
          {m.desktop_port_notice_open_desktop_settings()}
        </a>
      </div>
    </div>
  </section>
{:else if state?.mode === 'fallback'}
  <section class="border-l-2 border-warning bg-warning/5 px-4 py-3" aria-labelledby="desktop-port-warning-title">
    <div class="flex gap-3">
      <CircleAlertIcon class="mt-0.5 size-4 shrink-0 text-warning" />
      <div class="min-w-0 flex-1">
        <h2 id="desktop-port-warning-title" class="text-sm font-semibold">
          {m.desktop_port_notice_fixed_desktop_port_unavailable()}
        </h2>
        <p class="mt-1 text-sm text-muted-foreground">
          {m.desktop_port_notice_port()} <span class="font-technical tabular-nums">{state.fixedPort}</span>
          {m.desktop_port_notice_fallback_title()}
        </p>
        {#if state.bindingFailure}
          <p class="font-technical mt-1 text-xs text-muted-foreground">{state.bindingFailure.message}</p>
        {/if}
        {#if state.ownerLookup === 'identifying'}
          <p class="mt-1 text-sm text-muted-foreground">
            {m.common_identifying_occupying_application()}
          </p>
        {:else if state.ownerLookup === 'found'}
          <p class="font-technical mt-1 text-sm">{ownersLabel(state.owners)}</p>
        {:else if state.ownerLookup === 'unknown'}
          <p class="mt-1 text-sm text-muted-foreground">
            {m.common_occupying_application_not_identified()}
          </p>
        {/if}
        <a
          class={buttonVariants({ variant: 'outline', size: 'sm', class: 'mt-3' })}
          href={resolve('/settings#desktop')}>
          {m.desktop_port_notice_resolve_desktop_settings()}
        </a>
      </div>
    </div>
  </section>
{/if}
