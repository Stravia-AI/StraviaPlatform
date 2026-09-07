<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery } from '@tanstack/svelte-query'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'

import { getDesktopPortState } from '$lib/desktop-port'
import type { PortOwner } from '$lib/desktop-port'
import { buttonVariants } from '$lib/components/ui/button'
import * as Alert from '$lib/components/ui/alert'

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
  <Alert.Root variant="destructive" aria-labelledby="desktop-port-error-title">
    <CircleAlertIcon />
    <Alert.Title id="desktop-port-error-title" role="heading" aria-level={2}>
      {m.desktop_port_notice_desktop_port_setting_unavailable()}
    </Alert.Title>
    <Alert.Description>
      <p>
        {m.desktop_port_notice_fallback_reason()}
      </p>
      <p class="font-technical mt-1 text-xs text-muted-foreground">{state.configError}</p>
      <a class={buttonVariants({ variant: 'outline', size: 'sm', class: 'mt-3' })} href={resolve('/settings#desktop')}>
        {m.desktop_port_notice_open_desktop_settings()}
      </a>
    </Alert.Description>
  </Alert.Root>
{:else if state?.mode === 'fallback'}
  <Alert.Root variant="warning" role="status" aria-labelledby="desktop-port-warning-title">
    <CircleAlertIcon />
    <Alert.Title id="desktop-port-warning-title" role="heading" aria-level={2}>
      {m.desktop_port_notice_fixed_desktop_port_unavailable()}
    </Alert.Title>
    <Alert.Description>
      <p>
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
      <a class={buttonVariants({ variant: 'outline', size: 'sm', class: 'mt-3' })} href={resolve('/settings#desktop')}>
        {m.desktop_port_notice_resolve_desktop_settings()}
      </a>
    </Alert.Description>
  </Alert.Root>
{/if}
