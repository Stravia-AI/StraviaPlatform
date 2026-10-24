<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'

import { changeCredentials, getAuthState } from '#lib/auth.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'

const authStateQuery = createQuery(() => ({ queryKey: ['auth-state'], queryFn: getAuthState }))

let open = $state(false)
let currentPassword = $state('')
let adminUsername = $state('')
let newPassword = $state('')
let confirmPassword = $state('')
let saving = $state(false)
let error = $state('')

const username = $derived(authStateQuery.data?.username?.trim() ?? '')
const mismatch = $derived(confirmPassword.length > 0 && newPassword !== confirmPassword)
const incomplete = $derived(!currentPassword || !newPassword || newPassword !== confirmPassword)

function reset(): void {
  currentPassword = ''
  adminUsername = ''
  newPassword = ''
  confirmPassword = ''
  error = ''
}

function openEditor(): void {
  reset()
  open = true
}

async function save(): Promise<void> {
  const nextUsername = adminUsername.trim() || username
  if (!nextUsername || incomplete) return
  saving = true
  error = ''
  try {
    await changeCredentials(currentPassword, nextUsername, newPassword)
    window.location.assign('/login')
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<SettingsSection id="credentials" title={m.settings_admin_credentials()}>
  <SettingRow title={m.login_username()} value={username} onedit={openEditor} />
</SettingsSection>

<SettingsSheet
  bind:open
  title={m.settings_admin_credentials()}
  draft={() => [adminUsername, currentPassword, newPassword, confirmPassword]}
  submitLabel={m.settings_save_credentials()}
  footerNote={m.settings_credentials_sign_out()}
  {saving}
  submitDisabled={incomplete}
  {error}
  onclose={reset}
  onsubmit={save}>
  <Field.FieldGroup>
    <Field.Field data-invalid={error ? true : undefined}>
      <Field.FieldLabel for="current-password">{m.settings_current_password()}</Field.FieldLabel>
      <Input
        id="current-password"
        type="password"
        bind:value={currentPassword}
        disabled={saving}
        autocomplete="current-password"
        aria-invalid={error ? true : undefined} />
    </Field.Field>
    <Field.Field>
      <Field.FieldLabel for="admin-username" hint={m.settings_username_unchanged_help()}>
        {m.login_username()}
      </Field.FieldLabel>
      <Input
        id="admin-username"
        bind:value={adminUsername}
        disabled={saving}
        placeholder={username}
        autocomplete="username" />
    </Field.Field>
    <Field.Field>
      <Field.FieldLabel for="new-password">{m.settings_new_password()}</Field.FieldLabel>
      <Input id="new-password" type="password" bind:value={newPassword} disabled={saving} autocomplete="new-password" />
    </Field.Field>
    <Field.Field data-invalid={mismatch ? true : undefined}>
      <Field.FieldLabel for="confirm-password">{m.setup_confirm_password()}</Field.FieldLabel>
      <Input
        id="confirm-password"
        type="password"
        bind:value={confirmPassword}
        disabled={saving}
        autocomplete="new-password"
        aria-invalid={mismatch} />
      {#if mismatch}<Field.FieldError>{m.setup_passwords_must_match()}</Field.FieldError>{/if}
    </Field.Field>
  </Field.FieldGroup>
</SettingsSheet>
