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
import type { OAuthCallbackMode, OAuthSessionInitData } from '$lib/types'
import * as Field from '$lib/components/ui/field'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  driver: string
  useProxy: boolean
  mode: 'connect' | 'reconnect'
  providerName?: string
  onStateChange?: (sessionId: string | undefined, ready: boolean) => void
  class?: string
}

let { driver, useProxy, mode, providerName = '', onStateChange, class: className = '' }: Props = $props()
let oauthSession = $state<OAuthSessionInitData>()
let callbackUrl = $state('')
let callbackError = $state('')
let starting = $state(false)
let completing = $state(false)
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
  Boolean(oauthSession) && oauthStatus?.status !== 'ready' && oauthStatus?.status !== 'error',
)
const userCode = $derived(
  oauthStatus?.status === 'pending'
    ? (oauthStatus.user_code ?? oauthSession?.user_code)
    : oauthSession?.user_code,
)
const requiresManualCallback = $derived(
  oauthSession?.callback_mode === 'manual' || oauthSession?.listener_state === 'not_started',
)
const callbackInputId = $derived(mode === 'connect' ? 'oauth-callback-url' : 'provider-oauth-callback-url')

$effect(() => {
  const sessionId = oauthSession?.session_id
  const ready = oauthStatus?.status === 'ready'
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

export async function cancel(): Promise<void> {
  const sessionId = oauthSession?.session_id
  oauthSession = undefined
  callbackUrl = ''
  callbackError = ''
  if (!sessionId) return
  try {
    await admin.oauth.cancel(sessionId)
  } catch {
    // Session cleanup is best-effort; the server also expires abandoned OAuth sessions.
  }
}

async function begin(): Promise<void> {
  if (!driver) {
    toast.error(m.provider_oauth_authorization_unavailable())
    return
  }
  callbackError = ''
  const popup = typeof window === 'undefined' || isTauri ? null : window.open('about:blank', '_blank')
  starting = true
  await cancel()
  try {
    const session = await admin.oauth.init(driver, useProxy, callbackMode(), localeState.current)
    oauthSession = session
    if (popup) popup.location.replace(session.auth_url)
    else await openExternalUrl(session.auth_url)
  } catch (error) {
    popup?.close()
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    starting = false
  }
}

async function reopen(): Promise<void> {
  if (oauthSession) await openExternalUrl(oauthSession.auth_url)
}

async function completeManual(): Promise<void> {
  if (!oauthSession || !callbackUrl.trim()) return
  completing = true
  callbackError = ''
  try {
    await admin.oauth.complete(oauthSession.session_id, callbackUrl.trim())
    await oauthStatusQuery.refetch()
  } catch (error) {
    callbackError = localizeBackendErrorMessage(error)
  } finally {
    completing = false
  }
}

export async function updateProxy(nextUseProxy: boolean): Promise<void> {
  if (!oauthSession || !oauthInProgress) return
  try {
    await admin.oauth.updateProxy(oauthSession.session_id, nextUseProxy)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}

export function consume(): void {
  oauthSession = undefined
  callbackUrl = ''
  callbackError = ''
}

onDestroy(() => {
  void cancel()
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
            {mode === 'connect'
              ? m.provider_oauth_authorization_sign_service_browser()
              : m.provider_oauth_authorization_reconnect_warning()}
          </p>
        </div>
        <Button type="button" onclick={() => void begin()} disabled={starting}>
          {#if starting}<Spinner data-icon="inline-start" />{:else}<ExternalLinkIcon data-icon="inline-start" />{/if}
          {mode === 'connect'
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
              : oauthStatus?.status === 'error'
                ? m.provider_oauth_authorization_authorization_failed()
                : m.provider_oauth_authorization_waiting_authorization()}
          </p>
          {#if oauthStatus?.status === 'error'}
            <p class="mt-1 text-sm text-destructive">
              {localizeBackendErrorMessage(oauthStatus)}
            </p>
          {:else if oauthStatus?.status === 'pending' && oauthStatus.last_error}
            <p class="mt-1 text-sm text-destructive">
              {localizeBackendErrorMessage({ code: oauthStatus.error_code, message: oauthStatus.last_error })}
            </p>
          {:else if oauthInProgress}
            <p class="mt-1 text-sm text-muted-foreground">
              {m.provider_oauth_authorization_browser_help()}
            </p>
          {/if}
        </div>
        <div class="flex flex-wrap justify-end gap-2">
          {#if oauthStatus?.status === 'error'}
            <Button type="button" variant="outline" onclick={() => void begin()}>
              <RefreshCwIcon data-icon="inline-start" />{m.common_try_again()}
            </Button>
          {:else if oauthStatus?.status === 'ready'}
            <Button type="button" variant="outline" onclick={() => void begin()}>
              {m.provider_oauth_authorization_use_another_account()}
            </Button>
          {:else}
            <Button type="button" variant="outline" onclick={() => void reopen()}>
              <ExternalLinkIcon data-icon="inline-start" />{oauthStatus?.status === 'pending' && oauthStatus.last_error
                ? m.provider_oauth_authorization_try_sign_again()
                : m.provider_oauth_authorization_reopen_sign_page()}
            </Button>
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

      {#if requiresManualCallback && oauthInProgress}
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
