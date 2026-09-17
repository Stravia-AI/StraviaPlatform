<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SaveIcon from '@lucide/svelte/icons/save'
import { toast } from 'svelte-sonner'

import {
  asDesktopPortOperationError,
  getDesktopClientSettings,
  getDesktopPortState,
  recheckDesktopFixedPort,
  setDesktopExternalAccess,
  setDesktopFixedPort,
  setDesktopLaunchAtLogin,
  setDesktopSilentStart,
} from '$lib/desktop-client'
import type { DesktopClientSettings, DesktopPortOperationError, PortOwner } from '$lib/desktop-client'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as InputGroup from '$lib/components/ui/input-group'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const queryClient = useQueryClient()
const portQuery = createQuery(() => ({
  queryKey: ['desktop-port-state'],
  queryFn: getDesktopPortState,
  refetchInterval: (query) =>
    query.state.data?.ownerLookup === 'identifying' || query.state.data?.candidateError?.ownerLookup === 'identifying'
      ? 500
      : false,
}))
const clientQuery = createQuery(() => ({
  queryKey: ['desktop-client-settings'],
  queryFn: getDesktopClientSettings,
}))

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
let startupError = $state<string>()
let startupToggling = $state(false)
let lanSelection = $state<string>()

const portState = $derived(portQuery.data)
const clientState = $derived(clientQuery.data)
const baselinePort = $derived(portState ? String(portState.fixedPort ?? portState.currentPort) : '')
const portValue = $derived(portDraft ?? baselinePort)
const parsedPort = $derived(parsePort(portValue))
const canSave = $derived(portState != null && parsedPort != null && parsedPort !== portState.fixedPort)
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

function ownersLabel(owners: PortOwner[]): string {
  return owners.map((owner) => `${owner.name} (PID ${owner.pid})`).join(', ')
}

function requestSavePort(): void {
  if (parsedPort == null) {
    validationError = m.desktop_client_port_invalid()
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
  confirmationOpen = false
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

async function applyStartupToggle(
  apply: (enabled: boolean) => Promise<DesktopClientSettings>,
  enabled: boolean,
): Promise<void> {
  startupToggling = true
  startupError = undefined
  try {
    const next = await apply(enabled)
    queryClient.setQueryData(['desktop-client-settings'], next)
  } catch (error) {
    startupError = error instanceof Error ? error.message : String(error)
  } finally {
    startupToggling = false
  }
}
</script>

<section id="client" class="route-section scroll-mt-20 pb-8" aria-labelledby="client-title">
  <div class="route-section-header">
    <div>
      <h2 id="client-title" class="route-section-title">{m.desktop_client_title()}</h2>
      <p class="route-section-description">
        {m.desktop_client_summary()}
      </p>
    </div>
  </div>

  {#if portQuery.isPending || clientQuery.isPending}
    <div class="flex min-h-24 items-center justify-center border-y text-sm text-muted-foreground">
      <Spinner class="mr-2" />{m.desktop_client_loading()}
    </div>
  {:else if portQuery.isError}
    <div class="border-y py-4">
      <p class="text-sm font-medium text-destructive">{m.desktop_client_unavailable()}</p>
      <p class="mt-1 text-sm text-muted-foreground">{String(portQuery.error)}</p>
      <Button class="mt-3" variant="outline" onclick={() => void portQuery.refetch()}>
        {m.common_retry()}
      </Button>
    </div>
  {:else if portState}
    <Field.FieldGroup>
      <Field.Field size="fill" data-invalid={validationError != null || displayedOperationError != null}>
        <Field.FieldLabel for="desktop-fixed-port" hint={m.desktop_client_port_help()}
          >{m.desktop_client_port()}</Field.FieldLabel>
        <div class="flex min-w-0 flex-col gap-3">
          <div class="flex min-w-0 flex-col gap-2 sm:flex-row sm:flex-wrap">
            <InputGroup.Root class="min-w-0 sm:min-w-48 sm:flex-1">
              <InputGroup.Addon class="font-technical pr-0 tabular-nums">{bindHost}:</InputGroup.Addon>
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
              {m.desktop_client_save_port()}
            </Button>
            {#if portState.mode === 'fallback' && portState.fixedPort != null}
              <Button
                class="shrink-0"
                variant="outline"
                disabled={saving || rechecking}
                onclick={() => void recheckPort()}>
                {#if rechecking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
                    data-icon="inline-start" />{/if}
                {m.desktop_client_recheck_port()}
              </Button>
            {/if}
          </div>

          {#if portState.mode === 'fallback' && (portState.bindingFailure || portState.ownerLookup !== 'notApplicable')}
            <div class="border-s-2 border-warning ps-4 text-sm">
              <p>{m.desktop_client_using_temporary_port()}</p>
              {#if portState.bindingFailure}
                <p class="mt-1 text-muted-foreground">{portState.bindingFailure.message}</p>
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
            <div class="border-s-2 border-destructive ps-4 text-sm">
              <p>{m.desktop_client_port_settings_unreadable()}</p>
              <p class="mt-1 text-muted-foreground">{portState.configError}</p>
            </div>
          {/if}
        </div>
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

      <Field.Field orientation="horizontal" data-invalid={externalError != null}>
        <div class="flex-1">
          <Field.FieldLabel for="desktop-external-access" hint={m.desktop_client_external_access_hint()}
            >{m.desktop_client_external_access()}</Field.FieldLabel>
        </div>
        <Switch
          id="desktop-external-access"
          checked={portState.externalAccess}
          disabled={externalToggling || saving || rechecking}
          onCheckedChange={requestExternalAccess} />
      </Field.Field>
      {#if portState.externalAccess}
        <Field.Field orientation="horizontal">
          <div class="flex-1">
            <Field.FieldLabel for="desktop-external-address">{m.desktop_client_external_address()}</Field.FieldLabel>
          </div>
          {#if lanAddress}
            <InputGroup.Root class="w-fit max-w-full">
              <InputGroup.Addon class="font-technical pr-0 tabular-nums">http://</InputGroup.Addon>
              {#if lanAddresses.length > 1}
                <Select.Root type="single" value={lanAddress} onValueChange={(value) => (lanSelection = value)}>
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
                <InputGroup.Text class="font-technical px-1.5 text-foreground tabular-nums"
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
        </Field.Field>
      {/if}
      {#if externalError}
        <Field.FieldError>
          {externalError.code === 'bindFailed'
            ? m.desktop_client_external_not_switched()
            : m.desktop_client_settings_not_saved()}
          <span class="mt-1 block font-normal text-muted-foreground">{externalError.message}</span>
        </Field.FieldError>
      {/if}

      {#if clientState}
        <Field.Field orientation="horizontal" data-invalid={startupError != null}>
          <div class="flex-1">
            <Field.FieldLabel for="desktop-launch-at-login">{m.desktop_client_launch_at_login()}</Field.FieldLabel>
          </div>
          <Switch
            id="desktop-launch-at-login"
            checked={clientState.launchAtLogin}
            disabled={startupToggling}
            onCheckedChange={(enabled) => void applyStartupToggle(setDesktopLaunchAtLogin, enabled)} />
        </Field.Field>
        {#if clientState.launchAtLogin}
          <Field.Field orientation="horizontal">
            <div class="flex-1">
              <Field.FieldLabel for="desktop-silent-start" hint={m.desktop_client_silent_start_hint()}
                >{m.desktop_client_silent_start()}</Field.FieldLabel>
            </div>
            <Switch
              id="desktop-silent-start"
              checked={clientState.silentStart}
              disabled={startupToggling}
              onCheckedChange={(enabled) => void applyStartupToggle(setDesktopSilentStart, enabled)} />
          </Field.Field>
        {/if}
        {#if startupError}
          <Field.FieldError>
            {m.desktop_client_settings_not_saved()}
            <span class="mt-1 block font-normal text-muted-foreground">{startupError}</span>
          </Field.FieldError>
        {/if}
      {:else if clientQuery.isError}
        <p class="text-sm text-muted-foreground">
          {m.desktop_client_settings_unavailable()}
          <Button class="ms-2" variant="outline" size="sm" onclick={() => void clientQuery.refetch()}>
            {m.common_retry()}
          </Button>
        </p>
      {/if}
    </Field.FieldGroup>
  {/if}
</section>

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
      <AlertDialog.Action
        onclick={() => {
          externalConfirmOpen = false
          void applyExternalAccess(true)
        }}>
        {m.desktop_client_external_confirm_action()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
