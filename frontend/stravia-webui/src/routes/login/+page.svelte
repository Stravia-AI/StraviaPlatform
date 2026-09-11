<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import SecretInput from '$lib/components/secret-input.svelte'

import { login } from '$lib/auth'
import BrandMark from '$lib/components/brand-mark.svelte'
import BrandWordmark from '$lib/components/brand-wordmark.svelte'
import LanguageSelector from '$lib/components/language-selector.svelte'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'

let username = $state('')
let password = $state('')
let submitting = $state(false)
let errorKind = $state<'invalid' | 'unavailable'>()
let usernameInput = $state<HTMLInputElement | null>(null)

const error = $derived(
  errorKind === 'invalid'
    ? m.login_invalid_credentials()
    : errorKind === 'unavailable'
      ? m.login_server_unavailable()
      : undefined,
)

async function submit(): Promise<void> {
  const submittedUsername = username.trim()
  if (!submittedUsername || !password) return

  submitting = true
  errorKind = undefined
  try {
    await login(submittedUsername, password)
    window.location.replace('/')
  } catch (cause) {
    const status = (cause as { status?: number }).status
    errorKind = status === 400 || status === 401 ? 'invalid' : 'unavailable'
    usernameInput?.focus()
  } finally {
    submitting = false
  }
}
</script>

<svelte:head><title>{m.login_sign_stravia()} · Stravia</title></svelte:head>

<main class="grid place-items-center bg-background p-4 sm:p-8">
  <div class="grid w-full max-w-5xl overflow-hidden border-y bg-background min-[900px]:grid-cols-12 min-[900px]:border">
    <section
      class="flex flex-col justify-between border-b p-6 min-[900px]:col-span-5 min-[900px]:min-h-[34rem] min-[900px]:border-e min-[900px]:border-b-0 min-[900px]:p-10"
      aria-labelledby="login-brand-title">
      <div>
        <div class="flex items-center gap-3" aria-label="Stravia 观策行">
          <BrandMark class="size-12" state={submitting ? 'running' : 'static'} />
          <div>
            <p class="text-xl"><BrandWordmark /></p>
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
        <h2 id="login-form-title" class="font-structural mt-3 text-2xl font-semibold">{m.login_sign_stravia()}</h2>
        <p class="mt-2 text-sm text-muted-foreground">{m.login_enter_credentials()}</p>
        <form
          class="mt-7"
          onsubmit={(event) => {
            event.preventDefault()
            void submit()
          }}>
          <Field.FieldGroup>
            <Field.Field size="fill" data-invalid={error ? true : undefined}>
              <Field.FieldLabel for="admin-username">{m.login_username()}</Field.FieldLabel>
              <Input
                id="admin-username"
                bind:ref={usernameInput}
                bind:value={username}
                autocomplete="username"
                aria-invalid={error ? true : undefined}
                autofocus />
            </Field.Field>
            <Field.Field size="fill" data-invalid={error ? true : undefined}>
              <Field.FieldLabel for="admin-password">{m.login_password()}</Field.FieldLabel>
              <SecretInput
                id="admin-password"
                bind:value={password}
                autocomplete="current-password"
                showLabel={m.login_show_password()}
                hideLabel={m.login_hide_password()}
                aria-describedby={error ? 'login-password-error' : undefined}
                aria-invalid={error ? true : undefined} />
              {#if error}<Field.FieldError id="login-password-error">{error}</Field.FieldError>{/if}
            </Field.Field>
            <Button class="w-full" type="submit" disabled={submitting || !username.trim() || !password}
              >{#if submitting}<Spinner
                  data-icon="inline-start" />{m.login_signing_in()}{:else}{m.login_sign()}{/if}</Button>
          </Field.FieldGroup>
        </form>
      </div>
    </section>
  </div>
</main>
