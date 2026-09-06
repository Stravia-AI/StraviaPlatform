<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import CheckIcon from '@lucide/svelte/icons/check'

import { claimSetup, completeSetup, getAuthState, testDatabase, type DatabaseConfig } from '$lib/auth'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import BrandMark from '$lib/components/brand-mark.svelte'
import LanguageSelector from '$lib/components/language-selector.svelte'
import { Button } from '$lib/components/ui/button'
import * as Card from '$lib/components/ui/card'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { onMount } from 'svelte'

let loading = $state(true)
let authorized = $state(false)
let token = $state('')
let backend = $state<'sqlite' | 'postgres'>('sqlite')
let sqlitePath = $state('gateway.db')
let postgresUrl = $state('')
let maxConnections = $state('')
let minConnections = $state('')
let idleTimeoutSeconds = $state('')
let username = $state('')
let password = $state('')
let confirmingPassword = $state('')
let claiming = $state(false)
let testing = $state(false)
let completing = $state(false)
let databaseTested = $state(false)
let error = $state('')
let success = $state('')

const passwordsMatch = $derived(password === confirmingPassword)
const poolSettingsValid = $derived(
  [maxConnections, minConnections, idleTimeoutSeconds].every(
    (value) => !value.trim() || (/^\d+$/.test(value) && Number(value) > 0),
  ),
)
const databaseReady = $derived(
  backend === 'sqlite'
    ? /(?:^|[\\/])gateway\.db$/.test(sqlitePath.trim()) || sqlitePath.trim() === 'gateway.db'
    : postgresUrl.trim().length > 0 && poolSettingsValid,
)

onMount(() => {
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
  if (backend === 'sqlite') return { backend, path: sqlitePath.trim() }
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
  databaseTested = false
  error = ''
  success = ''
  try {
    await testDatabase(databaseConfig())
    databaseTested = true
    success = m.setup_connection_succeeded()
  } catch (cause) {
    error = errorMessage(cause)
  } finally {
    testing = false
  }
}

async function complete(): Promise<void> {
  if (!databaseReady || !databaseTested || !passwordsMatch) return
  completing = true
  error = ''
  success = ''
  try {
    await completeSetup(databaseConfig(), username.trim(), password)
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
  <div class="mx-auto flex w-full max-w-3xl flex-col gap-6">
    <header class="flex items-center justify-between gap-4 border-b pb-5">
      <div class="flex items-center gap-3">
        <BrandMark class="size-10" />
        <div>
          <p class="font-structural text-sm font-semibold tracking-[0.12em]">STRAVIA</p>
          <h1 class="font-structural text-2xl font-semibold">{m.setup_title()}</h1>
        </div>
      </div>
      <LanguageSelector description={false} />
    </header>

    {#if loading}
      <div class="flex min-h-64 items-center justify-center" role="status">
        <Spinner /> <span class="sr-only">{m.setup_loading()}</span>
      </div>
    {:else if !authorized}
      <Card.Root>
        <Card.Header>
          <Card.Title>{m.setup_claim_title()}</Card.Title>
          <Card.Description>{m.setup_claim_description()}</Card.Description>
        </Card.Header>
        <Card.Content>
          <form
            onsubmit={(event) => {
              event.preventDefault()
              void claim()
            }}>
            <Field.FieldGroup>
              <Field.Field data-invalid={error ? true : undefined}>
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
              <Button type="submit" disabled={claiming || !token.trim()}>
                {#if claiming}<Spinner data-icon="inline-start" />{/if}{m.setup_continue()}
              </Button>
            </Field.FieldGroup>
          </form>
        </Card.Content>
      </Card.Root>
    {:else}
      <Card.Root>
        <Card.Header>
          <Card.Title>{m.setup_database_title()}</Card.Title>
          <Card.Description>{m.setup_database_description()}</Card.Description>
        </Card.Header>
        <Card.Content>
          <Field.FieldGroup>
            <Field.Field size="select">
              <Field.FieldLabel for="database-backend">{m.setup_database_type()}</Field.FieldLabel>
              <Select.Root
                type="single"
                value={backend}
                onValueChange={(value) => {
                  if (value === 'sqlite' || value === 'postgres') {
                    backend = value
                    databaseTested = false
                    success = ''
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
            {#if backend === 'sqlite'}
              <Field.Field data-invalid={!databaseReady ? true : undefined}>
                <Field.FieldLabel for="sqlite-path">{m.setup_sqlite_path()}</Field.FieldLabel>
                <Input
                  id="sqlite-path"
                  class="font-technical"
                  bind:value={sqlitePath}
                  oninput={() => (databaseTested = false)}
                  aria-invalid={!databaseReady}
                  placeholder="/var/lib/stravia/gateway.db" />
                <Field.FieldDescription>{m.setup_sqlite_path_help()}</Field.FieldDescription>
              </Field.Field>
            {:else}
              <Field.Field data-invalid={!databaseReady ? true : undefined}>
                <Field.FieldLabel for="postgres-url">{m.setup_postgres_url()}</Field.FieldLabel>
                <Input
                  id="postgres-url"
                  class="font-technical"
                  type="password"
                  bind:value={postgresUrl}
                  oninput={() => (databaseTested = false)}
                  autocomplete="off"
                  aria-invalid={!databaseReady}
                  placeholder="postgresql://user:password@host/database" />
              </Field.Field>
              <div class="grid gap-4 sm:grid-cols-3">
                <Field.Field>
                  <Field.FieldLabel for="max-connections">{m.setup_max_connections()}</Field.FieldLabel>
                  <Input
                    id="max-connections"
                    type="number"
                    min="1"
                    bind:value={maxConnections}
                    oninput={() => (databaseTested = false)} />
                </Field.Field>
                <Field.Field>
                  <Field.FieldLabel for="min-connections">{m.setup_min_connections()}</Field.FieldLabel>
                  <Input
                    id="min-connections"
                    type="number"
                    min="1"
                    bind:value={minConnections}
                    oninput={() => (databaseTested = false)} />
                </Field.Field>
                <Field.Field>
                  <Field.FieldLabel for="idle-timeout">{m.setup_idle_timeout()}</Field.FieldLabel>
                  <Input
                    id="idle-timeout"
                    type="number"
                    min="1"
                    bind:value={idleTimeoutSeconds}
                    oninput={() => (databaseTested = false)} />
                </Field.Field>
              </div>
            {/if}
            <Button
              type="button"
              variant="outline"
              disabled={testing || !databaseReady}
              onclick={() => void testConnection()}>
              {#if testing}<Spinner data-icon="inline-start" />{:else if databaseTested}<CheckIcon
                  data-icon="inline-start" />{/if}
              {testing ? m.setup_testing_connection() : m.setup_test_connection()}
            </Button>
          </Field.FieldGroup>
        </Card.Content>
      </Card.Root>

      <Card.Root>
        <Card.Header>
          <Card.Title>{m.setup_admin_title()}</Card.Title>
          <Card.Description>{m.setup_admin_description()}</Card.Description>
        </Card.Header>
        <Card.Content>
          <form
            onsubmit={(event) => {
              event.preventDefault()
              void complete()
            }}>
            <Field.FieldGroup>
              <Field.Field>
                <Field.FieldLabel for="setup-username">{m.login_username()}</Field.FieldLabel>
                <Input id="setup-username" bind:value={username} autocomplete="username" />
              </Field.Field>
              <Field.Field>
                <Field.FieldLabel for="setup-password">{m.login_password()}</Field.FieldLabel>
                <Input id="setup-password" type="password" bind:value={password} autocomplete="new-password" />
              </Field.Field>
              <Field.Field data-invalid={confirmingPassword.length > 0 && !passwordsMatch ? true : undefined}>
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
              {#if error}<Field.FieldError>{error}</Field.FieldError>{/if}
              {#if success}<p class="text-sm text-muted-foreground" role="status">{success}</p>{/if}
              <Button type="submit" disabled={completing || !databaseReady || !databaseTested || !passwordsMatch}>
                {#if completing}<Spinner data-icon="inline-start" />{/if}{m.setup_complete()}
              </Button>
            </Field.FieldGroup>
          </form>
        </Card.Content>
      </Card.Root>
    {/if}
  </div>
</main>
