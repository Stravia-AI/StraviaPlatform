<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount } from 'svelte'
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'

import { getAdminToken, setAdminToken } from '$lib/auth'
import { isTauri } from '$lib/admin-client'
import BrandMark from '$lib/components/brand-mark.svelte'
import LanguageSelector from '$lib/components/language-selector.svelte'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'

let token = $state('')
let showToken = $state(false)
let submitting = $state(false)
let errorKind = $state<'invalid' | 'unavailable'>()
let tokenInput = $state<HTMLInputElement | null>(null)

const copy = $derived({
  title: m.login_sign_stravia(),
  subtitle: m.login_enter_admin_token_stravia_server(),
  placeholder: m.login_enter_admin_token(),
  signIn: m.login_sign(),
  verifying: m.login_verifying(),
  invalid: m.login_invalid_token_check_configured_admin_token(),
  unavailable: m.login_cannot_reach_local_stravia_server_check_running(),
})
const error = $derived(
  errorKind === 'invalid' ? copy.invalid : errorKind === 'unavailable' ? copy.unavailable : undefined,
)

onMount(() => {
  if (isTauri) {
    window.location.replace('/')
    return
  }

  void (async () => {
    try {
      const existingToken = getAdminToken()
      const response = await fetch('/api/v1/status', {
        headers: existingToken ? { Authorization: `Bearer ${existingToken}` } : undefined,
      })
      if (response.ok) window.location.replace('/')
    } catch {
      // Keep the login form available when the local Server is unavailable.
    }
  })()
})

async function submit(): Promise<void> {
  const submittedToken = token.trim()
  if (!submittedToken) return

  submitting = true
  errorKind = undefined
  try {
    const response = await fetch('/api/v1/status', { headers: { Authorization: `Bearer ${submittedToken}` } })
    if (response.ok) {
      setAdminToken(submittedToken)
      window.location.replace('/')
      return
    }
    errorKind = response.status === 401 ? 'invalid' : 'unavailable'
    tokenInput?.focus()
  } catch {
    errorKind = 'unavailable'
  } finally {
    submitting = false
  }
}
</script>

<svelte:head><title>{copy.signIn} · Stravia</title></svelte:head>

<main class="grid place-items-center bg-background p-4 sm:p-8">
  <div class="grid w-full max-w-5xl overflow-hidden border-y bg-background min-[900px]:grid-cols-12 min-[900px]:border">
    <section
      class="flex flex-col justify-between border-b p-6 min-[900px]:col-span-5 min-[900px]:min-h-[34rem] min-[900px]:border-e min-[900px]:border-b-0 min-[900px]:p-10"
      aria-labelledby="login-brand-title">
      <div>
        <div class="flex items-center gap-3" aria-label="Stravia 观策行">
          <BrandMark class="size-12" />
          <div>
            <p class="font-structural text-sm font-semibold tracking-[0.12em]">STRAVIA</p>
            <p class="mt-0.5 text-xs tracking-[0.08em] text-muted-foreground">观策行</p>
          </div>
        </div>
        <h1
          id="login-brand-title"
          class="font-structural mt-8 text-[1.875rem] leading-8 font-semibold tracking-[-0.025em] text-balance">
          {m.login_manage_local_ai_gateway()}
        </h1>
        <p class="mt-3 max-w-sm text-sm leading-6 text-pretty text-muted-foreground">
          {m.login_product_summary()}
        </p>
      </div>
      <LanguageSelector class="mt-8 max-w-sm" />
    </section>

    <section class="flex items-center p-6 min-[900px]:col-span-7 min-[900px]:p-12" aria-labelledby="login-form-title">
      <div class="mx-auto w-full max-w-md">
        <p class="font-structural text-[0.72rem] font-semibold tracking-[0.14em] text-primary uppercase">
          {m.login_sign()}
        </p>
        <h2 id="login-form-title" class="font-structural mt-3 text-2xl font-semibold">{copy.title}</h2>
        <p class="mt-2 text-sm text-muted-foreground">{copy.subtitle}</p>
        <form
          class="mt-7"
          onsubmit={(event) => {
            event.preventDefault()
            void submit()
          }}>
          <Field.FieldGroup>
            <Field.Field size="fill" data-invalid={error ? true : undefined}>
              <Field.FieldLabel for="admin-token">{m.login_admin_token_label()}</Field.FieldLabel>
              <div class="flex gap-2">
                <Input
                  id="admin-token"
                  class="font-technical"
                  bind:ref={tokenInput}
                  bind:value={token}
                  type={showToken ? 'text' : 'password'}
                  placeholder={copy.placeholder}
                  autocomplete="current-password"
                  aria-invalid={error ? true : undefined}
                  autofocus />
                <Button
                  type="button"
                  variant="outline"
                  size="icon"
                  onclick={() => (showToken = !showToken)}
                  aria-label={showToken ? m.login_hide_token() : m.login_show_token()}
                  >{#if showToken}<EyeOffIcon />{:else}<EyeIcon />{/if}</Button>
              </div>
              {#if error}<Field.FieldError>{error}</Field.FieldError>{/if}
            </Field.Field>
            <Button class="w-full" type="submit" disabled={submitting || !token.trim()}
              >{#if submitting}<Spinner data-icon="inline-start" />{copy.verifying}{:else}{copy.signIn}{/if}</Button>
          </Field.FieldGroup>
        </form>
      </div>
    </section>
  </div>
</main>
