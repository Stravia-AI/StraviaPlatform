<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import { onDestroy } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin, isTauri } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { localeState } from '$lib/localization.svelte'
import { openExternalUrl } from '$lib/open-external'
import type { OAuthCallbackMode, OAuthCandidateConfiguration, OAuthSessionInitData } from '$lib/types'
import * as Field from '$lib/components/ui/field'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import SecretInput from '$lib/components/secret-input.svelte'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  vendorId: string
  channel: string
  flow: 'authorization_code' | 'device_code' | 'manual'
  configuration: OAuthCandidateConfiguration
  useProxy: boolean
  mode: 'connect' | 'reconnect'
  providerName?: string
  onStateChange?: (sessionId: string | undefined, ready: boolean) => void
  class?: string
}

let {
  vendorId,
  channel,
  flow,
  configuration,
  useProxy,
  mode,
  providerName = '',
  onStateChange,
  class: className = '',
}: Props = $props()
let oauthSession = $state<OAuthSessionInitData>()
let callbackUrl = $state('')
let manualValue = $state('')
let callbackError = $state('')
let starting = $state(false)
let completing = $state(false)
let sessionGeneration = 0
let reportedSessionId: string | undefined
let reportedReady = false

const oauthStatusQuery = createQuery(() => ({
  queryKey: ['oauth-session', oauthSession?.session_id],
  queryFn: () => admin.oauth.status(oauthSession!.session_id),
  enabled: Boolean(oauthSession),
  refetchInterval: (query) => {
    const status = query.state.data?.status
    return status === 'ready' || status === 'error' ? false : (oauthSession?.interval ?? 2) * 1000
  },
}))
const oauthStatus = $derived(oauthStatusQuery.data)
const oauthInProgress = $derived(
  Boolean(oauthSession) &&
    !oauthStatusQuery.isError &&
    oauthStatus?.status !== 'ready' &&
    oauthStatus?.status !== 'error',
)
const userCode = $derived(
  oauthStatus?.status === 'pending' ? (oauthStatus.user_code ?? oauthSession?.user_code) : oauthSession?.user_code,
)
const currentAuthUrl = $derived(
  oauthStatus?.status === 'pending' ? (oauthStatus.auth_url ?? oauthSession?.auth_url) : oauthSession?.auth_url,
)
const fallbackReason = $derived(
  oauthStatus?.status === 'pending'
    ? (oauthStatus.fallback_reason ?? oauthSession?.fallback_reason)
    : oauthSession?.fallback_reason,
)
const manualInput = $derived(
  oauthStatus?.status === 'pending'
    ? (oauthStatus.manual_input ?? oauthSession?.manual_input)
    : oauthSession?.manual_input,
)
const supportsManualCallback = $derived(
  flow === 'authorization_code' && oauthSession?.callback_mode === 'manual' && !manualInput,
)
const supportsManualInput = $derived(Boolean(manualInput) && oauthInProgress)
const callbackInputId = $derived(mode === 'connect' ? 'oauth-callback-url' : 'provider-oauth-callback-url')
const manualInputId = $derived(mode === 'connect' ? 'oauth-manual-value' : 'provider-oauth-manual-value')

$effect(() => {
  const sessionId = oauthSession?.session_id
  const ready = Boolean(sessionId) && oauthStatus?.status === 'ready'
  if (sessionId === reportedSessionId && ready === reportedReady) return
  reportedSessionId = sessionId
  reportedReady = ready
  onStateChange?.(sessionId, ready)
})

function callbackMode(): OAuthCallbackMode {
  if (isTauri) return 'auto'
  const hostname = window.location.hostname
  return hostname === 'localhost' ||
    hostname === '::1' ||
    hostname === '[::1]' ||
    /^127(?:\.\d{1,3}){3}$/.test(hostname)
    ? 'auto'
    : 'manual'
}

function resetLocalSession(): string | undefined {
  const sessionId = oauthSession?.session_id
  oauthSession = undefined
  callbackUrl = ''
  manualValue = ''
  callbackError = ''
  completing = false
  return sessionId
}

function isCurrentSession(generation: number, sessionId: string): boolean {
  return sessionGeneration === generation && oauthSession?.session_id === sessionId
}

export async function cancel(): Promise<void> {
  sessionGeneration += 1
  starting = false
  const sessionId = resetLocalSession()
  if (!sessionId) return
  try {
    await admin.oauth.cancel(sessionId)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}

async function begin(): Promise<void> {
  if (starting) return
  if (!vendorId || !channel) {
    toast.error(m.provider_oauth_authorization_unavailable())
    return
  }
  const popup =
    flow === 'manual' || typeof window === 'undefined' || isTauri ? null : window.open('about:blank', '_blank')
  const generation = ++sessionGeneration
  const previousSessionId = resetLocalSession()
  starting = true
  if (previousSessionId) {
    try {
      await admin.oauth.cancel(previousSessionId)
    } catch (error) {
      if (generation === sessionGeneration) toast.error(localizeBackendErrorMessage(error))
    }
  }
  if (generation !== sessionGeneration) {
    popup?.close()
    return
  }
  try {
    const session = await admin.oauth.init(
      vendorId,
      channel,
      configuration,
      useProxy,
      callbackMode(),
      localeState.current,
    )
    if (generation !== sessionGeneration) {
      popup?.close()
      void admin.oauth.cancel(session.session_id).catch(() => undefined)
      return
    }
    oauthSession = session
    if (session.auth_url) {
      if (popup) popup.location.replace(session.auth_url)
      else await openExternalUrl(session.auth_url)
    } else {
      popup?.close()
    }
  } catch (error) {
    popup?.close()
    if (generation === sessionGeneration) toast.error(localizeBackendErrorMessage(error))
  } finally {
    if (generation === sessionGeneration) starting = false
  }
}

async function reopen(): Promise<void> {
  if (currentAuthUrl) await openExternalUrl(currentAuthUrl)
}

async function completeManual(): Promise<void> {
  const sessionId = oauthSession?.session_id
  const value = callbackUrl.trim()
  if (!sessionId || !value) return
  const generation = sessionGeneration
  completing = true
  callbackError = ''
  try {
    await admin.oauth.complete(sessionId, 'callback_url', value)
    if (!isCurrentSession(generation, sessionId)) return
    await oauthStatusQuery.refetch()
  } catch (error) {
    if (isCurrentSession(generation, sessionId)) callbackError = localizeBackendErrorMessage(error)
  } finally {
    if (isCurrentSession(generation, sessionId)) completing = false
  }
}

async function completeManualInput(): Promise<void> {
  const sessionId = oauthSession?.session_id
  const inputType = manualInput?.type
  const value = manualValue.trim()
  if (!sessionId || !inputType || !value) return
  const generation = sessionGeneration
  completing = true
  callbackError = ''
  try {
    await admin.oauth.complete(sessionId, inputType === 'callback_url' ? 'callback_url' : 'manual', value)
    if (!isCurrentSession(generation, sessionId)) return
    manualValue = ''
    await oauthStatusQuery.refetch()
  } catch (error) {
    if (isCurrentSession(generation, sessionId)) callbackError = localizeBackendErrorMessage(error)
  } finally {
    if (isCurrentSession(generation, sessionId)) completing = false
  }
}

export async function updateProxy(nextUseProxy: boolean): Promise<void> {
  const sessionId = oauthSession?.session_id
  if (!sessionId) return
  if (oauthStatus?.status === 'ready' || oauthStatus?.status === 'error') {
    await cancel()
    return
  }
  const generation = sessionGeneration
  try {
    await admin.oauth.updateProxy(sessionId, nextUseProxy)
  } catch (error) {
    if (!isCurrentSession(generation, sessionId)) return
    toast.error(localizeBackendErrorMessage(error))
    await cancel()
  }
}

export function consume(): void {
  sessionGeneration += 1
  starting = false
  resetLocalSession()
}

onDestroy(() => {
  sessionGeneration += 1
  starting = false
  const sessionId = resetLocalSession()
  if (sessionId) void admin.oauth.cancel(sessionId).catch(() => undefined)
})
</script>

<Field.Set class={className}>
  <Field.Legend>{m.provider_oauth_authorization_account_authorization()}</Field.Legend>
  <div class="rounded-xl border bg-muted/20 p-4">
    {#if !oauthSession}
      <div class="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p class="font-medium">
            {mode === 'connect' ? `${providerName} OAuth` : m.common_oauth_account()}
          </p>
          <p class="mt-1 text-sm text-muted-foreground">
            {flow === 'manual'
              ? m.provider_oauth_authorization_manual_help()
              : mode === 'connect'
                ? m.provider_oauth_authorization_sign_service_browser()
                : m.provider_oauth_authorization_reconnect_warning()}
          </p>
        </div>
        <Button type="button" onclick={() => void begin()} disabled={starting}>
          {#if starting}<Spinner data-icon="inline-start" />{:else if flow !== 'manual'}<ExternalLinkIcon
              data-icon="inline-start" />{/if}
          {flow === 'manual'
            ? m.provider_oauth_authorization_begin_manual()
            : mode === 'connect'
              ? m.provider_oauth_authorization_sign_oauth()
              : m.provider_oauth_authorization_sign_again()}
        </Button>
      </div>
    {:else}
      <div class="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p class="font-medium">
            {oauthStatus?.status === 'ready'
              ? m.provider_oauth_authorization_authorization_complete()
              : oauthStatus?.status === 'error' || oauthStatusQuery.isError
                ? m.provider_oauth_authorization_authorization_failed()
                : m.provider_oauth_authorization_waiting_authorization()}
          </p>
          {#if oauthStatusQuery.isError}
            <p class="mt-1 text-sm text-destructive">
              {localizeBackendErrorMessage(oauthStatusQuery.error)}
            </p>
          {:else if oauthStatus?.status === 'error'}
            <p class="mt-1 text-sm text-destructive">
              {localizeBackendErrorMessage(oauthStatus)}
            </p>
          {:else if oauthStatus?.status === 'pending' && oauthStatus.last_error}
            <p class="mt-1 text-sm text-destructive">
              {localizeBackendErrorMessage({ code: oauthStatus.error_code, message: oauthStatus.last_error })}
            </p>
          {:else if oauthInProgress}
            <p class="mt-1 text-sm text-muted-foreground">
              {flow === 'manual'
                ? m.provider_oauth_authorization_manual_waiting_help()
                : m.provider_oauth_authorization_browser_help()}
            </p>
            {#if fallbackReason}
              <p class="mt-1 text-sm text-muted-foreground">
                {m.provider_oauth_authorization_fallback_reason({ reason: fallbackReason })}
              </p>
            {/if}
          {/if}
        </div>
        <div class="flex flex-wrap justify-end gap-2">
          {#if oauthStatus?.status === 'error' || oauthStatusQuery.isError}
            <Button type="button" variant="outline" onclick={() => void begin()}>
              <RefreshCwIcon data-icon="inline-start" />{m.common_try_again()}
            </Button>
          {:else if oauthStatus?.status === 'ready'}
            <Button type="button" variant="outline" onclick={() => void begin()}>
              {m.provider_oauth_authorization_use_another_account()}
            </Button>
          {:else}
            {#if currentAuthUrl}
              <Button type="button" variant="outline" onclick={() => void reopen()}>
                <ExternalLinkIcon data-icon="inline-start" />{oauthStatus?.status === 'pending' &&
                oauthStatus.last_error
                  ? m.provider_oauth_authorization_try_sign_again()
                  : m.provider_oauth_authorization_reopen_sign_page()}
              </Button>
            {/if}
            <Button type="button" variant="ghost" onclick={() => void cancel()}>
              {m.provider_oauth_authorization_cancel_sign()}
            </Button>
          {/if}
        </div>
      </div>

      {#if userCode && oauthInProgress}
        <div class="mt-4 rounded-lg border bg-background px-4 py-3">
          <p class="text-xs font-medium text-muted-foreground">
            {m.provider_oauth_authorization_device_code()}
          </p>
          <code class="mt-1 block font-technical text-lg font-semibold tracking-widest">{userCode}</code>
          <p class="mt-1 text-xs text-muted-foreground">
            {m.provider_oauth_authorization_device_code_help()}
          </p>
        </div>
      {/if}

      {#if supportsManualInput && manualInput}
        <Field.Field size="fill" class="mt-4">
          <Field.Label for={manualInputId} hint={manualInput.description ?? undefined}>
            {manualInput.label}
          </Field.Label>
          <div class="flex gap-2">
            <div class="min-w-0 flex-1">
              {#if manualInput.secret}
                <SecretInput
                  id={manualInputId}
                  class="font-technical"
                  bind:value={manualValue}
                  resetKey={oauthSession.session_id}
                  autocomplete="off"
                  oninput={() => (callbackError = '')} />
              {:else}
                <Input
                  id={manualInputId}
                  class="font-technical"
                  bind:value={manualValue}
                  autocomplete="off"
                  oninput={() => (callbackError = '')} />
              {/if}
            </div>
            <Button
              type="button"
              variant="outline"
              onclick={() => void completeManualInput()}
              disabled={completing || !manualValue.trim()}>
              {#if completing}<Spinner data-icon="inline-start" />{/if}
              {m.provider_oauth_authorization_complete()}
            </Button>
          </div>
          {#if callbackError}<Field.Error>{callbackError}</Field.Error>{/if}
        </Field.Field>
      {/if}

      {#if supportsManualCallback && oauthInProgress}
        <Field.Field size="fill" class="mt-4">
          <Field.Label for={callbackInputId} hint={m.provider_oauth_authorization_manual_callback_help()}>
            {m.provider_oauth_authorization_callback_url()}
          </Field.Label>
          <div class="flex gap-2">
            <Input
              id={callbackInputId}
              class="font-technical"
              bind:value={callbackUrl}
              oninput={() => (callbackError = '')}
              placeholder="http://localhost:1457/auth/callback?code=…&state=…" />
            <Button
              type="button"
              variant="outline"
              onclick={() => void completeManual()}
              disabled={completing || !callbackUrl.trim()}>
              {#if completing}<Spinner data-icon="inline-start" />{/if}
              {m.provider_oauth_authorization_complete()}
            </Button>
          </div>
          {#if callbackError}<Field.Error>{callbackError}</Field.Error>{/if}
        </Field.Field>
      {/if}
    {/if}
  </div>
</Field.Set>
