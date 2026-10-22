<script lang="ts">
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import * as m from '#lib/paraglide/messages.js'
import SecretInput from '#lib/components/secret-input.svelte'

import { login } from '#lib/auth.js'
import BrandMark from '#lib/components/brand-mark.svelte'
import BrandWordmark from '#lib/components/brand-wordmark.svelte'
import LanguageSelector from '#lib/components/language-selector.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

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
    await goto(resolve('/'), { replace: true })
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
  <div class="flex w-full max-w-md flex-col gap-8">
    <header class="flex items-center justify-between gap-4">
      <div class="flex min-w-0 items-center gap-2" aria-label="Stravia 观策行">
        <BrandMark class="size-8" state={submitting ? 'running' : 'static'} />
        <p class="text-lg"><BrandWordmark /></p>
        <p class="text-xs tracking-[0.08em] text-muted-foreground">观策行</p>
      </div>
      <LanguageSelector compact class="shrink-0" />
    </header>

    <section aria-labelledby="login-title">
      <h1 id="login-title" class="font-structural text-2xl font-semibold">{m.login_sign_stravia()}</h1>
      <form
        class="mt-6"
        onsubmit={(event) => {
          event.preventDefault()
          void submit()
        }}>
        <Field.FieldGroup>
          <Field.Field orientation="vertical" size="fill" data-invalid={error ? true : undefined}>
            <Field.FieldLabel for="admin-username">{m.login_username()}</Field.FieldLabel>
            <Input
              id="admin-username"
              bind:ref={usernameInput}
              bind:value={username}
              autocomplete="username"
              aria-invalid={error ? true : undefined}
              autofocus />
          </Field.Field>
          <Field.Field orientation="vertical" size="fill" data-invalid={error ? true : undefined}>
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
    </section>
  </div>
</main>
