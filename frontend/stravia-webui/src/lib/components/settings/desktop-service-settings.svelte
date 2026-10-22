<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import { toast } from 'svelte-sonner'

import {
  asDesktopPortOperationError,
  getDesktopClientSettings,
  getDesktopPortState,
  recheckDesktopFixedPort,
  setDesktopExternalAccess,
  setDesktopFixedPort,
} from '#lib/desktop-client.js'
import type { DesktopPortOperationError, PortOwner } from '#lib/desktop-client.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import { inputValue } from '#lib/utils.js'

const queryClient = useQueryClient()
const portQuery = createQuery(() => ({
  queryKey: ['desktop-port-state'],
  queryFn: getDesktopPortState,
  refetchInterval: (query) =>
    query.state.data?.ownerLookup === 'identifying' || query.state.data?.candidateError?.ownerLookup === 'identifying'
      ? 500
      : false,
}))
const clientQuery = createQuery(() => ({ queryKey: ['desktop-client-settings'], queryFn: getDesktopClientSettings }))

let portDraft = $state<string>()
let validationError = $state<string>()
let operationError = $state<DesktopPortOperationError>()
let saving = $state(false)
let rechecking = $state(false)
let confirmationOpen = $state(false)
let pendingPort = $state<number>()

let externalConfirmOpen = $state(false)
let externalError = $state<DesktopPortOperationError>()
let externalToggling = $state(false)
let lanSelection = $state<string>()

const portState = $derived(portQuery.data)
const clientState = $derived(clientQuery.data)
const baselinePort = $derived(portState ? String(portState.fixedPort ?? portState.currentPort) : '')
const portValue = $derived(portDraft ?? baselinePort)
const parsedPort = $derived(parsePort(portValue))
const canSave = $derived(portState != null && parsedPort != null && parsedPort !== portState.fixedPort)
const portDirty = $derived(portDraft !== undefined && portDraft !== baselinePort)
const displayedOperationError = $derived(
  operationError &&
    (portState?.candidatePort === parsedPort ? (portState?.candidateError ?? operationError) : operationError),
)
const bindHost = $derived(portState?.externalAccess ? '0.0.0.0' : '127.0.0.1')
const lanAddresses = $derived(clientState?.lanAddresses ?? [])
const lanAddress = $derived(
  lanSelection && lanAddresses.includes(lanSelection) ? lanSelection : (lanAddresses[0] ?? ''),
)
const lanUrl = $derived(lanAddress && portState ? `http://${lanAddress}:${portState.currentPort}` : '')

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

function revertPort(): void {
  portDraft = undefined
  validationError = undefined
  operationError = undefined
}

function ownersLabel(owners: PortOwner[]): string {
  return owners.map((owner) => `${owner.name} (PID ${owner.pid})`).join(', ')
}

function requestSavePort(): void {
  if (saving || rechecking) return
  if (parsedPort == null) {
    validationError = m.desktop_client_port_invalid()
    return
  }
  if (parsedPort === portState?.fixedPort) return

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

function onPortKeydown(event: KeyboardEvent): void {
  if (event.isComposing) return
  if (event.key === 'Enter') {
    event.preventDefault()
    requestSavePort()
  } else if (event.key === 'Escape' && portDirty) {
    event.preventDefault()
    event.stopPropagation()
    revertPort()
  }
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

function requestExternalAccess(enabled: boolean): void {
  if (enabled) {
    externalConfirmOpen = true
    return
  }
  void applyExternalAccess(false)
}

async function applyExternalAccess(enabled: boolean): Promise<void> {
  externalToggling = true
  externalError = undefined
  try {
    const nextState = await setDesktopExternalAccess(enabled)
    queryClient.setQueryData(['desktop-port-state'], nextState)
    void clientQuery.refetch()
  } catch (error) {
    externalError = asDesktopPortOperationError(error) ?? {
      code: 'bindFailed',
      message: error instanceof Error ? error.message : String(error),
      bindingFailure: null,
      ownerLookup: 'notApplicable',
      owners: [],
    }
  } finally {
    externalToggling = false
  }
}

async function copyLanUrl(): Promise<void> {
  try {
    await navigator.clipboard.writeText(lanUrl)
    toast.success(m.common_copied_clipboard())
  } catch {
    toast.error(m.common_not_copy_clipboard())
  }
}
</script>

<SettingsSection id="client" title={m.desktop_client_title()}>
  {#if portQuery.isPending}
    <p role="status" class="flex items-center gap-2 py-3 text-sm text-muted-foreground">
      <Spinner />{m.desktop_client_loading()}
    </p>
  {:else if portQuery.isError}
    <div class="py-3" role="alert">
      <p class="text-sm font-medium text-destructive">{m.desktop_client_unavailable()}</p>
      <p class="mt-1 text-sm text-muted-foreground">{String(portQuery.error)}</p>
      <Button class="mt-3" variant="outline" onclick={() => void portQuery.refetch()}>
        {m.common_retry()}
      </Button>
    </div>
  {:else if portState}
    <SettingRow
      title={m.desktop_client_port()}
      labelFor="desktop-fixed-port"
      description={m.desktop_client_port_help()}
      controlSize="name">
      <div
        class="flex w-full min-w-0 flex-col gap-2"
        data-invalid={validationError != null || displayedOperationError != null}>
        <InputGroup.Root class="min-w-0">
          <InputGroup.Addon class="font-technical pr-0 tabular-nums">{bindHost}:</InputGroup.Addon>
          <InputGroup.Input
            id="desktop-fixed-port"
            class="font-technical min-w-16 tabular-nums"
            inputmode="numeric"
            min="1024"
            max="65535"
            value={portValue}
            disabled={saving}
            aria-invalid={validationError != null || displayedOperationError != null}
            oninput={(event: Event) => setPortDraft(inputValue(event))}
            onkeydown={onPortKeydown} />
        </InputGroup.Root>
        {#if portDirty || saving || (portState.mode === 'fallback' && portState.fixedPort != null)}
          <div class="flex flex-wrap items-center justify-end gap-2">
            {#if portDirty || saving}
              <Button
                size="sm"
                disabled={!canSave || saving || rechecking}
                aria-busy={saving}
                onclick={requestSavePort}>
                {#if saving}<Spinner data-icon="inline-start" />{/if}
                {m.desktop_client_save_port()}
              </Button>
              <Button size="sm" variant="ghost" disabled={saving} onclick={revertPort}>{m.common_cancel()}</Button>
            {/if}
            {#if portState.mode === 'fallback' && portState.fixedPort != null}
              <Button size="sm" variant="outline" disabled={saving || rechecking} onclick={() => void recheckPort()}>
                {#if rechecking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
                    data-icon="inline-start" />{/if}
                {m.desktop_client_recheck_port()}
              </Button>
            {/if}
          </div>
        {/if}
      </div>
    </SettingRow>
    {#if portState.mode === 'fallback' && (portState.bindingFailure || portState.ownerLookup !== 'notApplicable')}
      <div class="mb-2 border-s-2 border-warning ps-4 text-sm" role="status">
        <p>{m.desktop_client_using_temporary_port()}</p>
        {#if portState.bindingFailure}
          <p class="mt-1 text-muted-foreground">{portState.bindingFailure.message}</p>
        {/if}
        {#if portState.ownerLookup === 'identifying'}
          <p class="mt-1 text-muted-foreground">{m.common_identifying_occupying_application()}</p>
        {:else if portState.ownerLookup === 'found'}
          <p class="font-technical mt-1">{ownersLabel(portState.owners)}</p>
        {:else if portState.ownerLookup === 'unknown'}
          <p class="mt-1 text-muted-foreground">{m.common_occupying_application_not_identified()}</p>
        {/if}
      </div>
    {:else if portState.mode === 'configError'}
      <div class="mb-2 border-s-2 border-destructive ps-4 text-sm" role="alert">
        <p>{m.desktop_client_port_settings_unreadable()}</p>
        <p class="mt-1 text-muted-foreground">{portState.configError}</p>
      </div>
    {/if}
    {#if validationError}<Field.FieldError>{validationError}</Field.FieldError>{/if}
    {#if displayedOperationError}
      <Field.FieldError>
        {displayedOperationError.code === 'bindFailed'
          ? m.desktop_client_port_not_bound()
          : displayedOperationError.code === 'storeWriteFailed'
            ? m.desktop_client_port_not_saved()
            : displayedOperationError.message}
        <span class="mt-1 block font-normal text-muted-foreground">{displayedOperationError.message}</span>
        {#if displayedOperationError.ownerLookup === 'identifying'}
          <span class="mt-1 block font-normal text-muted-foreground">
            {m.common_identifying_occupying_application()}
          </span>
        {:else if displayedOperationError.ownerLookup === 'found'}
          <span class="mt-1 block font-technical font-normal">{ownersLabel(displayedOperationError.owners)}</span>
        {:else if displayedOperationError.ownerLookup === 'unknown'}
          <span class="mt-1 block font-normal">{m.common_occupying_application_not_identified()}</span>
        {/if}
      </Field.FieldError>
    {/if}

    <SettingRow
      layout="inline"
      title={m.desktop_client_external_access()}
      labelFor="desktop-external-access"
      description={m.desktop_client_external_access_hint()}>
      {#if externalToggling}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="desktop-external-access"
        bind:checked={() => portState.externalAccess, requestExternalAccess}
        disabled={externalToggling || saving || rechecking} />
    </SettingRow>
    {#if portState.externalAccess}
      <SettingRow nested title={m.desktop_client_external_address()} labelFor="desktop-external-address">
        {#if lanAddress}
          <InputGroup.Root class="w-fit max-w-full">
            <InputGroup.Addon class="font-technical pr-0 tabular-nums">http://</InputGroup.Addon>
            {#if lanAddresses.length > 1}
              <Select.Root type="single" value={lanAddress} onValueChange={(value: string) => (lanSelection = value)}>
                <Select.Trigger
                  id="desktop-external-address"
                  class="rounded-md border-transparent px-1.5 py-0 font-technical text-sm tabular-nums hover:bg-accent focus-visible:border-transparent focus-visible:ring-2 data-[size=default]:h-8 dark:bg-transparent dark:hover:bg-accent">
                  {lanAddress}
                </Select.Trigger>
                <Select.Content>
                  {#each lanAddresses as address (address)}
                    <Select.Item value={address} label={address} />
                  {/each}
                </Select.Content>
              </Select.Root>
            {:else}
              <InputGroup.Text id="desktop-external-address" class="font-technical px-1.5 text-foreground tabular-nums"
                >{lanAddress}</InputGroup.Text>
            {/if}
            <InputGroup.Addon align="inline-end">
              <span class="font-technical tabular-nums">:{portState.currentPort}</span>
              <InputGroup.Button
                size="icon-sm"
                aria-label={m.technical_value_copy_full_value()}
                onclick={() => void copyLanUrl()}>
                <ClipboardCopyIcon />
              </InputGroup.Button>
            </InputGroup.Addon>
          </InputGroup.Root>
        {:else}
          <p class="text-sm text-muted-foreground">{m.desktop_client_external_no_address()}</p>
        {/if}
      </SettingRow>
    {/if}
    {#if externalError}
      <Field.FieldError>
        {externalError.code === 'bindFailed'
          ? m.desktop_client_external_not_switched()
          : m.desktop_client_settings_not_saved()}
        <span class="mt-1 block font-normal text-muted-foreground">{externalError.message}</span>
      </Field.FieldError>
    {/if}
  {/if}
</SettingsSection>

<AlertDialog.Root bind:open={confirmationOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.desktop_client_change_port_title()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.desktop_client_change_confirmation({
          current: `${bindHost}:${portState?.currentPort ?? '—'}`,
          pending: `${bindHost}:${pendingPort ?? '—'}`,
        })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (pendingPort = undefined)}>
        {m.desktop_client_keep_current_port()}
      </AlertDialog.Cancel>
      <AlertDialog.Action onclick={confirmPortChange}>{m.desktop_client_change_port()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={externalConfirmOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.desktop_client_external_confirm_title()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.desktop_client_external_confirm_description()}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action onclick={() => void applyExternalAccess(true)}>
        {m.desktop_client_external_confirm_action()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
