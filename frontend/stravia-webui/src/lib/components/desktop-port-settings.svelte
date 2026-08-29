<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SaveIcon from '@lucide/svelte/icons/save'

import {
  asDesktopPortOperationError,
  getDesktopPortState,
  recheckDesktopFixedPort,
  setDesktopFixedPort,
} from '$lib/desktop-port'
import type { DesktopPortOperationError, PortOwner } from '$lib/desktop-port'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as InputGroup from '$lib/components/ui/input-group'
import { Spinner } from '$lib/components/ui/spinner'

const queryClient = useQueryClient()
const portQuery = createQuery(() => ({
  queryKey: ['desktop-port-state'],
  queryFn: getDesktopPortState,
  refetchInterval: (query) =>
    query.state.data?.ownerLookup === 'identifying' || query.state.data?.candidateError?.ownerLookup === 'identifying'
      ? 500
      : false,
}))

let portDraft = $state<string>()
let validationError = $state<string>()
let operationError = $state<DesktopPortOperationError>()
let saving = $state(false)
let rechecking = $state(false)
let confirmationOpen = $state(false)
let pendingPort = $state<number>()

const portState = $derived(portQuery.data)
const baselinePort = $derived(portState ? String(portState.fixedPort ?? portState.currentPort) : '')
const portValue = $derived(portDraft ?? baselinePort)
const parsedPort = $derived(parsePort(portValue))
const canSave = $derived(portState != null && parsedPort != null && parsedPort !== portState.fixedPort)
const displayedOperationError = $derived(
  operationError &&
    (portState?.candidatePort === parsedPort ? (portState?.candidateError ?? operationError) : operationError),
)

function parsePort(value: string): number | undefined {
  if (!/^\d+$/.test(value.trim())) return undefined
  const port = Number(value)
  return Number.isInteger(port) && port >= 1024 && port <= 65535 ? port : undefined
}

function setPortDraft(value: string): void {
  portDraft = value
  validationError = undefined
  operationError = undefined
}

function ownersLabel(owners: PortOwner[]): string {
  return owners.map((owner) => `${owner.name} (PID ${owner.pid})`).join(', ')
}

function requestSavePort(): void {
  if (parsedPort == null) {
    validationError = m.desktop_port_settings_invalid_port()
    return
  }

  if (portState?.mode === 'fixed' && parsedPort !== portState.currentPort) {
    pendingPort = parsedPort
    confirmationOpen = true
    return
  }

  void savePort(parsedPort)
}

function confirmPortChange(): void {
  const port = pendingPort
  pendingPort = undefined
  if (port != null) void savePort(port)
}

async function savePort(port: number): Promise<void> {
  saving = true
  validationError = undefined
  operationError = undefined
  try {
    const nextState = await setDesktopFixedPort(port)
    queryClient.setQueryData(['desktop-port-state'], nextState)
    portDraft = undefined
  } catch (error) {
    operationError = asDesktopPortOperationError(error)
    if (operationError?.ownerLookup === 'identifying') void portQuery.refetch()
    if (!operationError) {
      validationError = error instanceof Error ? error.message : String(error)
    }
  } finally {
    saving = false
  }
}

async function recheckPort(): Promise<void> {
  rechecking = true
  validationError = undefined
  operationError = undefined
  try {
    const nextState = await recheckDesktopFixedPort()
    queryClient.setQueryData(['desktop-port-state'], nextState)
    portDraft = undefined
  } catch (error) {
    operationError = asDesktopPortOperationError(error)
    if (!operationError) {
      validationError = error instanceof Error ? error.message : String(error)
    }
  } finally {
    rechecking = false
  }
}
</script>

<section id="desktop" class="route-section scroll-mt-20 pb-8" aria-labelledby="desktop-title">
  <div class="route-section-header">
    <div>
      <h2 id="desktop-title" class="route-section-title">{m.desktop_port_settings_desktop()}</h2>
      <p class="route-section-description">
        {m.desktop_port_settings_summary()}
      </p>
    </div>
  </div>

  {#if portQuery.isPending}
    <div class="flex min-h-24 items-center justify-center border-y text-sm text-muted-foreground">
      <Spinner class="mr-2" />{m.desktop_port_settings_loading_desktop_port()}
    </div>
  {:else if portQuery.isError}
    <div class="border-y py-4">
      <p class="text-sm font-medium text-destructive">{m.desktop_port_settings_desktop_port_unavailable()}</p>
      <p class="mt-1 text-sm text-muted-foreground">{String(portQuery.error)}</p>
      <Button class="mt-3" variant="outline" onclick={() => void portQuery.refetch()}>
        {m.common_retry()}
      </Button>
    </div>
  {:else if portState}
    <Field.FieldGroup>
      <Field.Field size="fill" data-invalid={validationError != null || displayedOperationError != null}>
        <Field.FieldLabel for="desktop-fixed-port" hint={m.desktop_port_settings_port_help()}
          >{m.desktop_port_settings_fixed_port()}</Field.FieldLabel>
        <div class="flex min-w-0 flex-col gap-3">
          <div class="flex min-w-0 flex-col gap-2 sm:flex-row sm:flex-wrap">
            <InputGroup.Root class="min-w-0 sm:min-w-48 sm:flex-1">
              <InputGroup.Addon class="font-technical pr-0 tabular-nums">127.0.0.1:</InputGroup.Addon>
              <InputGroup.Input
                id="desktop-fixed-port"
                class="font-technical min-w-16 tabular-nums"
                inputmode="numeric"
                min="1024"
                max="65535"
                value={portValue}
                aria-invalid={validationError != null || displayedOperationError != null}
                oninput={(event) => setPortDraft(event.currentTarget.value)} />
            </InputGroup.Root>
            <Button class="shrink-0" disabled={!canSave || saving || rechecking} onclick={requestSavePort}>
              {#if saving}<Spinner data-icon="inline-start" />{:else}<SaveIcon data-icon="inline-start" />{/if}
              {m.desktop_port_settings_save_fixed_port()}
            </Button>
            {#if portState.mode === 'fallback' && portState.fixedPort != null}
              <Button
                class="shrink-0"
                variant="outline"
                disabled={saving || rechecking}
                onclick={() => void recheckPort()}>
                {#if rechecking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
                    data-icon="inline-start" />{/if}
                {m.desktop_port_settings_recheck_fixed_port()}
              </Button>
            {/if}
          </div>

          <div class="flex min-h-6 flex-wrap items-center justify-between gap-x-4 gap-y-1">
            <StatusIndicator
              compact
              label={portState.mode === 'fixed'
                ? m.desktop_port_settings_fixed_port_active()
                : portState.mode === 'fallback'
                  ? m.desktop_port_settings_stravia_using_temporary_random_port()
                  : m.desktop_port_settings_desktop_port_setting_not_read()}
              tone={portState.mode === 'fixed' ? 'healthy' : portState.mode === 'fallback' ? 'warning' : 'error'} />
            <p class="text-xs text-muted-foreground">
              {m.desktop_port_settings_current_listener()}
              <span class="font-technical ms-1 tabular-nums text-foreground">127.0.0.1:{portState.currentPort}</span>
            </p>
          </div>

          {#if portState.mode === 'fallback' && (portState.bindingFailure || portState.ownerLookup !== 'notApplicable')}
            <div class="border-s-2 border-warning ps-4 text-sm">
              {#if portState.bindingFailure}
                <p class="text-muted-foreground">{portState.bindingFailure.message}</p>
              {/if}
              {#if portState.ownerLookup === 'identifying'}
                <p class="mt-1 text-muted-foreground">
                  {m.common_identifying_occupying_application()}
                </p>
              {:else if portState.ownerLookup === 'found'}
                <p class="font-technical mt-1">{ownersLabel(portState.owners)}</p>
              {:else if portState.ownerLookup === 'unknown'}
                <p class="mt-1 text-muted-foreground">
                  {m.common_occupying_application_not_identified()}
                </p>
              {/if}
            </div>
          {:else if portState.mode === 'configError'}
            <p class="border-s-2 border-destructive ps-4 text-sm text-muted-foreground">
              {portState.configError}
            </p>
          {/if}
        </div>
        {#if validationError}<Field.FieldError>{validationError}</Field.FieldError>{/if}
        {#if displayedOperationError}
          <Field.FieldError>
            {displayedOperationError.code === 'bindFailed'
              ? m.desktop_port_settings_port_not_bound()
              : displayedOperationError.code === 'storeWriteFailed'
                ? m.desktop_port_settings_port_not_saved()
                : displayedOperationError.message}
            <span class="mt-1 block font-normal text-muted-foreground">{displayedOperationError.message}</span>
            {#if displayedOperationError.ownerLookup === 'identifying'}
              <span class="mt-1 block font-normal text-muted-foreground">
                {m.common_identifying_occupying_application()}
              </span>
            {:else if displayedOperationError.ownerLookup === 'found'}
              <span class="mt-1 block font-technical font-normal">
                {ownersLabel(displayedOperationError.owners)}
              </span>
            {:else if displayedOperationError.ownerLookup === 'unknown'}
              <span class="mt-1 block font-normal">
                {m.common_occupying_application_not_identified()}
              </span>
            {/if}
          </Field.FieldError>
        {/if}
      </Field.Field>
    </Field.FieldGroup>
  {/if}
</section>

<AlertDialog.Root bind:open={confirmationOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.desktop_port_settings_change_fixed_desktop_port()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.desktop_port_settings_change_confirmation({
          currentPort: portState?.currentPort ?? '—',
          pendingPort: pendingPort ?? '—',
        })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (pendingPort = undefined)}>
        {m.desktop_port_settings_keep_current_port()}
      </AlertDialog.Cancel>
      <AlertDialog.Action onclick={confirmPortChange}>{m.desktop_port_settings_change_port()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
