<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import CheckIcon from '@lucide/svelte/icons/check'

import { claimSetup, completeSetup, getAuthState, testDatabase, type DatabaseConfig } from '#lib/auth.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import BrandMark from '#lib/components/brand-mark.svelte'
import BrandWordmark from '#lib/components/brand-wordmark.svelte'
import LanguageSelector from '#lib/components/language-selector.svelte'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as Collapsible from '#lib/components/ui/collapsible/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { onMount } from 'svelte'

let loading = $state(true)
let authorized = $state(false)
let token = $state('')
let backend = $state<'sqlite' | 'postgres'>('sqlite')
let postgresUrl = $state('')
let maxConnections = $state('')
let minConnections = $state('')
let idleTimeoutSeconds = $state('')
let clientBaseUrl = $state('')
let username = $state('')
let password = $state('')
let confirmingPassword = $state('')
let claiming = $state(false)
let testing = $state(false)
let completing = $state(false)
let databaseTested = $state(false)
let error = $state('')
let testError = $state('')
let advancedOpen = $state(false)

const passwordsMatch = $derived(password === confirmingPassword)
const poolSettingsValid = $derived(
  [maxConnections, minConnections, idleTimeoutSeconds].every(
    (value) => !value.trim() || (/^\d+$/.test(value) && Number(value) > 0),
  ),
)
const poolInvalid = $derived(backend === 'postgres' && !poolSettingsValid)
const databaseReady = $derived(backend === 'sqlite' || (postgresUrl.trim().length > 0 && poolSettingsValid))

onMount(() => {
  clientBaseUrl = `${window.location.origin}${window.location.pathname.replace(/\/setup\/?$/, '').replace(/\/$/, '')}`
  void getAuthState()
    .then((state) => {
      if (state.mode !== 'setup') {
        window.location.replace(state.mode === 'server' ? '/login' : '/')
        return
      }
      authorized = state.setup_authorized
    })
    .catch((cause) => (error = errorMessage(cause)))
    .finally(() => (loading = false))
})

function errorMessage(cause: unknown): string {
  return localizeBackendErrorMessage(cause)
}

function optionalPositiveInteger(value: string): number | undefined {
  if (!value.trim()) return undefined
  const parsed = Number.parseInt(value, 10)
  return Number.isInteger(parsed) && parsed > 0 ? parsed : undefined
}

function databaseConfig(): DatabaseConfig {
  if (backend === 'sqlite') return { backend }
  return {
    backend,
    url: postgresUrl.trim(),
    ...(optionalPositiveInteger(maxConnections) ? { max_connections: optionalPositiveInteger(maxConnections) } : {}),
    ...(optionalPositiveInteger(minConnections) ? { min_connections: optionalPositiveInteger(minConnections) } : {}),
    ...(optionalPositiveInteger(idleTimeoutSeconds)
      ? { idle_timeout_seconds: optionalPositiveInteger(idleTimeoutSeconds) }
      : {}),
  }
}

// 数据库类型、URL 或连接池任一变化，都必须重新通过真实连接测试。
function invalidateDatabaseTest(): void {
  databaseTested = false
  testError = ''
}

async function claim(): Promise<void> {
  if (!token.trim()) return
  claiming = true
  error = ''
  try {
    await claimSetup(token.trim())
    authorized = true
    token = ''
  } catch (cause) {
    error = errorMessage(cause)
  } finally {
    claiming = false
  }
}

async function testConnection(): Promise<void> {
  if (!databaseReady) return
  testing = true
  invalidateDatabaseTest()
  try {
    await testDatabase(databaseConfig())
    databaseTested = true
  } catch (cause) {
    testError = errorMessage(cause)
  } finally {
    testing = false
  }
}

async function complete(): Promise<void> {
  if (!databaseReady || !databaseTested || !passwordsMatch) return
  completing = true
  error = ''
  try {
    await completeSetup(databaseConfig(), username.trim(), password, clientBaseUrl)
    window.location.replace('/login')
  } catch (cause) {
    error = errorMessage(cause)
  } finally {
    completing = false
  }
}
</script>

<svelte:head><title>{m.setup_title()} · Stravia</title></svelte:head>

<main class="bg-background p-4 sm:p-8">
  <div class="mx-auto flex w-full max-w-3xl flex-col gap-8">
    <header class="flex flex-col gap-5 border-b pb-6">
      <div class="flex items-center justify-between gap-4">
        <div class="flex min-w-0 items-center gap-2">
          <BrandMark
            class="size-8"
            state={loading || claiming || testing || completing ? 'running' : databaseTested ? 'complete' : 'static'} />
          <p class="text-lg"><BrandWordmark /></p>
        </div>
        <LanguageSelector compact class="shrink-0" />
      </div>
      <h1 class="font-structural text-2xl font-semibold text-balance">{m.setup_title()}</h1>
    </header>

    {#if loading}
      <div class="flex min-h-64 items-center justify-center" role="status">
        <Spinner /> <span class="sr-only">{m.setup_loading()}</span>
      </div>
    {:else if !authorized}
      <section class="flex flex-col gap-4" aria-labelledby="setup-claim-title">
        <div class="flex flex-col gap-1">
          <h2 id="setup-claim-title" class="font-structural text-lg font-semibold">{m.setup_claim_title()}</h2>
          <p class="text-sm text-muted-foreground">{m.setup_claim_description()}</p>
        </div>
        <form
          onsubmit={(event) => {
            event.preventDefault()
            void claim()
          }}>
          <Field.FieldGroup>
            <Field.Field orientation="vertical" data-invalid={error ? true : undefined}>
              <Field.FieldLabel for="setup-token">{m.setup_token()}</Field.FieldLabel>
              <Input
                id="setup-token"
                class="font-technical"
                type="password"
                bind:value={token}
                autocomplete="off"
                aria-invalid={error ? true : undefined}
                autofocus />
              <Field.FieldDescription>{m.setup_token_help()}</Field.FieldDescription>
              {#if error}<Field.FieldError>{error}</Field.FieldError>{/if}
            </Field.Field>
            <Button class="self-start" type="submit" disabled={claiming || !token.trim()}>
              {#if claiming}<Spinner data-icon="inline-start" />{/if}{m.setup_continue()}
            </Button>
          </Field.FieldGroup>
        </form>
      </section>
    {:else}
      <!-- 数据库类型、连接池与管理员凭据是同一份资源草稿：不即时保存，只由「完成设置」整体提交。 -->
      <form
        class="flex flex-col gap-8"
        onsubmit={(event) => {
          event.preventDefault()
          void complete()
        }}>
        <section class="flex flex-col gap-3" aria-labelledby="setup-database-title">
          <div class="flex items-center gap-2">
            <h2 id="setup-database-title" class="font-structural text-lg font-semibold">{m.setup_database_title()}</h2>
            <Field.FieldHelp hint={m.setup_database_description()} />
          </div>
          <Field.FieldGroup>
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="database-backend">{m.setup_database_type()}</Field.FieldLabel>
              <Select.Root
                type="single"
                value={backend}
                onValueChange={(value: string) => {
                  if (value === 'sqlite' || value === 'postgres') {
                    backend = value
                    invalidateDatabaseTest()
                  }
                }}>
                <Select.Trigger id="database-backend" class="w-full">
                  {backend === 'sqlite' ? 'SQLite' : 'PostgreSQL'}
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    <Select.Item value="sqlite" label="SQLite">SQLite</Select.Item>
                    <Select.Item value="postgres" label="PostgreSQL">PostgreSQL</Select.Item>
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            </Field.Field>
            {#if backend === 'postgres'}
              <Field.Field orientation="vertical" data-invalid={!databaseReady ? true : undefined}>
                <Field.FieldLabel for="postgres-url">{m.setup_postgres_url()}</Field.FieldLabel>
                <Input
                  id="postgres-url"
                  class="font-technical"
                  type="password"
                  bind:value={postgresUrl}
                  oninput={invalidateDatabaseTest}
                  autocomplete="off"
                  aria-invalid={!databaseReady}
                  placeholder="postgresql://user:password@host/database" />
              </Field.Field>
              <Collapsible.Root bind:open={advancedOpen} class="flex flex-col gap-3">
                <Collapsible.Trigger class={buttonVariants({ variant: 'outline', class: 'self-start' })}>
                  {m.common_advanced()}
                </Collapsible.Trigger>
                <Collapsible.Content>
                  <div class="grid gap-4 sm:grid-cols-3">
                    <Field.Field orientation="vertical">
                      <Field.FieldLabel for="max-connections">{m.setup_max_connections()}</Field.FieldLabel>
                      <Input
                        id="max-connections"
                        class="w-28"
                        type="text"
                        inputmode="numeric"
                        bind:value={maxConnections}
                        oninput={invalidateDatabaseTest} />
                    </Field.Field>
                    <Field.Field orientation="vertical">
                      <Field.FieldLabel for="min-connections">{m.setup_min_connections()}</Field.FieldLabel>
                      <Input
                        id="min-connections"
                        class="w-28"
                        type="text"
                        inputmode="numeric"
                        bind:value={minConnections}
                        oninput={invalidateDatabaseTest} />
                    </Field.Field>
                    <Field.Field orientation="vertical">
                      <Field.FieldLabel for="idle-timeout">{m.setup_idle_timeout()}</Field.FieldLabel>
                      <Input
                        id="idle-timeout"
                        class="w-28"
                        type="text"
                        inputmode="numeric"
                        bind:value={idleTimeoutSeconds}
                        oninput={invalidateDatabaseTest} />
                    </Field.Field>
                  </div>
                </Collapsible.Content>
              </Collapsible.Root>
              {#if poolInvalid}<Field.FieldError>{m.setup_pool_invalid()}</Field.FieldError>{/if}
            {/if}
            <div class="flex flex-col items-start gap-2">
              <Button
                type="button"
                variant="outline"
                disabled={testing || !databaseReady}
                onclick={() => void testConnection()}>
                {#if testing}<Spinner data-icon="inline-start" />{:else if databaseTested}<CheckIcon
                    data-icon="inline-start" />{/if}
                {testing ? m.setup_testing_connection() : m.setup_test_connection()}
              </Button>
              {#if databaseTested}
                <p class="text-sm text-muted-foreground" role="status">{m.setup_connection_succeeded()}</p>
              {:else if testError}
                <Field.FieldError>{testError}</Field.FieldError>
              {/if}
            </div>
          </Field.FieldGroup>
        </section>

        <section class="flex flex-col gap-3" aria-labelledby="setup-admin-title">
          <h2 id="setup-admin-title" class="font-structural text-lg font-semibold">{m.setup_admin_title()}</h2>
          <Field.FieldGroup>
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="setup-client-base-url">{m.artifact_client_base_url()}</Field.FieldLabel>
              <Field.FieldDescription>{m.artifact_client_base_help()}</Field.FieldDescription>
              <Input id="setup-client-base-url" type="url" required bind:value={clientBaseUrl} />
            </Field.Field>
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="setup-username">{m.login_username()}</Field.FieldLabel>
              <Input id="setup-username" bind:value={username} autocomplete="username" />
            </Field.Field>
            <Field.Field orientation="vertical">
              <Field.FieldLabel for="setup-password">{m.login_password()}</Field.FieldLabel>
              <Input id="setup-password" type="password" bind:value={password} autocomplete="new-password" />
            </Field.Field>
            <Field.Field
              orientation="vertical"
              data-invalid={confirmingPassword.length > 0 && !passwordsMatch ? true : undefined}>
              <Field.FieldLabel for="setup-confirm-password">{m.setup_confirm_password()}</Field.FieldLabel>
              <Input
                id="setup-confirm-password"
                type="password"
                bind:value={confirmingPassword}
                autocomplete="new-password"
                aria-invalid={confirmingPassword.length > 0 && !passwordsMatch} />
              {#if confirmingPassword.length > 0 && !passwordsMatch}
                <Field.FieldError>{m.setup_passwords_must_match()}</Field.FieldError>
              {/if}
            </Field.Field>
          </Field.FieldGroup>
        </section>

        <div class="flex flex-col items-start gap-3">
          {#if error}<Field.FieldError>{error}</Field.FieldError>{/if}
          {#if !databaseTested}<p class="text-sm text-muted-foreground">{m.setup_test_required()}</p>{/if}
          <Button type="submit" disabled={completing || !databaseReady || !databaseTested || !passwordsMatch}>
            {#if completing}<Spinner data-icon="inline-start" />{/if}{m.setup_complete()}
          </Button>
        </div>
      </form>
    {/if}
  </div>
</main>
