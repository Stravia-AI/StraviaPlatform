<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin, type ArtifactS3Settings, type ArtifactSettings } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import InlineSettingInput from '#lib/components/inline-setting-input.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SecretInput from '#lib/components/secret-input.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

const queryClient = useQueryClient()
const artifactQuery = createQuery(() => ({ queryKey: ['artifact-settings'], queryFn: admin.settings.artifacts }))
const artifact = $derived(artifactQuery.data)
const needsAddress = $derived(artifact !== undefined && !artifact.client_base_url)

let busyKey = $state<'storage' | 'external' | 'upload'>()
let rowError = $state('')

async function persist(patch: Partial<ArtifactSettings>): Promise<ArtifactSettings> {
  if (!artifact) throw new Error(m.artifact_load_failed())
  const value = { ...artifact, ...patch }
  await admin.settings.saveArtifacts(value)
  queryClient.setQueryData(['artifact-settings'], value)
  return value
}

async function saveImmediately(key: 'storage' | 'external' | 'upload', patch: Partial<ArtifactSettings>) {
  busyKey = key
  rowError = ''
  try {
    await persist(patch)
  } catch (cause) {
    rowError = localizeBackendErrorMessage(cause)
  } finally {
    busyKey = undefined
  }
}

async function saveClientAddress(next: string): Promise<string> {
  return (await persist({ client_base_url: next.trim() })).client_base_url
}

// --- S3 Sheet ------------------------------------------------------------
const emptyS3: ArtifactS3Settings = {
  endpoint: '',
  region: '',
  bucket: '',
  access_key_id: '',
  secret_access_key: '',
  session_token: null,
  credentials_expires_at: null,
}

let s3Open = $state(false)
let s3Draft = $state<ArtifactS3Settings>({ ...emptyS3 })
let s3ExpiryText = $state('')
let s3ExpiryInitial = ''
let s3Saving = $state(false)
let s3Error = $state('')

function pad(value: number): string {
  return String(value).padStart(2, '0')
}

function toLocalDatetime(ms: number | null | undefined): string {
  if (ms == null) return ''
  const date = new Date(ms)
  if (Number.isNaN(date.getTime())) return ''
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function openS3Editor(): void {
  s3Draft = { ...emptyS3, ...artifact?.s3 }
  s3ExpiryText = toLocalDatetime(s3Draft.credentials_expires_at)
  s3ExpiryInitial = s3ExpiryText
  s3Error = ''
  s3Open = true
}

async function saveS3(): Promise<void> {
  s3Saving = true
  s3Error = ''
  try {
    // 未改动的到期时间保留原毫秒值；改动后以本地时间换算。
    const expires =
      s3ExpiryText === s3ExpiryInitial
        ? (s3Draft.credentials_expires_at ?? null)
        : s3ExpiryText === ''
          ? null
          : new Date(s3ExpiryText).getTime()
    await persist({ s3: { ...s3Draft, session_token: s3Draft.session_token || null, credentials_expires_at: expires } })
    s3Open = false
    toast.success(m.artifact_saved())
  } catch (cause) {
    s3Error = localizeBackendErrorMessage(cause)
  } finally {
    s3Saving = false
  }
}

function selectStorage(value: string): void {
  if (!artifact) return
  const current = artifact.s3 ? 's3' : 'internal'
  if (value === current) return
  if (value === 's3') openS3Editor()
  else void saveImmediately('storage', { s3: null })
}

// --- External signed downloads Sheet --------------------------------------
let downloadsOpen = $state(false)
let publicUrlDraft = $state('')
let downloadsSaving = $state(false)
let downloadsError = $state('')

function openDownloadsEditor(): void {
  publicUrlDraft = artifact?.file_public_base_url || artifact?.s3?.endpoint || artifact?.client_base_url || ''
  downloadsError = ''
  downloadsOpen = true
}

async function saveDownloads(): Promise<void> {
  downloadsSaving = true
  downloadsError = ''
  try {
    await persist({ external_signed_downloads: true, file_public_base_url: publicUrlDraft.trim() })
    downloadsOpen = false
    toast.success(m.artifact_saved())
  } catch (cause) {
    downloadsError = localizeBackendErrorMessage(cause)
  } finally {
    downloadsSaving = false
  }
}

function toggleExternalDownloads(enabled: boolean): void {
  if (enabled) openDownloadsEditor()
  else void saveImmediately('external', { external_signed_downloads: false })
}
</script>

<SettingsSection id="artifacts" title={m.artifact_settings_title()} badge={m.common_beta()}>
  {#if artifactQuery.error}
    <RequestFailure
      title={m.artifact_load_failed()}
      message={localizeBackendErrorMessage(artifactQuery.error)}
      retry={() => void artifactQuery.refetch()}
      retrying={artifactQuery.isFetching} />
  {:else if artifact}
    <SettingRow
      title={m.artifact_client_base_url()}
      labelFor="artifact-client-base-url"
      description={m.artifact_client_base_help()}
      controlSize="fill">
      {#snippet children({ titleId, descriptionId })}
        <InlineSettingInput
          id="artifact-client-base-url"
          type="url"
          technical
          labelledby={titleId}
          describedby={descriptionId}
          value={artifact.client_base_url}
          onSave={saveClientAddress} />
      {/snippet}
    </SettingRow>

    <SettingRow title={m.artifact_storage()} labelFor="artifact-storage" controlSize="setting">
      <Select.Root
        type="single"
        value={artifact.s3 ? 's3' : 'internal'}
        onValueChange={selectStorage}
        disabled={busyKey === 'storage'}>
        <Select.Trigger id="artifact-storage" class="w-full" aria-busy={busyKey === 'storage' || undefined}>
          {artifact.s3 ? m.artifact_s3_storage() : m.artifact_internal_storage()}
        </Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="internal">{m.artifact_internal_storage()}</Select.Item>
            <Select.Item value="s3">{m.artifact_s3_storage()}</Select.Item>
          </Select.Group>
        </Select.Content>
      </Select.Root>
    </SettingRow>
    {#if artifact.s3}
      <SettingRow nested title={m.artifact_s3_settings()} value={artifact.s3.bucket} onedit={openS3Editor} />
    {/if}

    <SettingRow
      layout="inline"
      title={m.artifact_external_downloads()}
      labelFor="artifact-external-downloads"
      description={needsAddress ? m.artifact_save_address_first() : undefined}>
      {#if busyKey === 'external'}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="artifact-external-downloads"
        bind:checked={() => artifact.external_signed_downloads, toggleExternalDownloads}
        disabled={busyKey === 'external' || needsAddress} />
    </SettingRow>
    {#if artifact.external_signed_downloads}
      <SettingRow
        nested
        title={m.artifact_public_base_url()}
        value={artifact.file_public_base_url ?? ''}
        onedit={openDownloadsEditor} />
    {/if}

    <SettingRow
      layout="inline"
      title={m.artifact_upload_injection()}
      labelFor="artifact-upload-injection"
      description={needsAddress ? m.artifact_save_address_first() : m.artifact_upload_help()}>
      {#if busyKey === 'upload'}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="artifact-upload-injection"
        bind:checked={
          () => artifact.upload_prompt_injection,
          (enabled: boolean) => void saveImmediately('upload', { upload_prompt_injection: enabled })
        }
        disabled={busyKey === 'upload' || needsAddress} />
    </SettingRow>
    {#if rowError}<Field.FieldError role="alert">{rowError}</Field.FieldError>{/if}
  {:else}
    <div class="flex flex-col gap-3 py-3" aria-busy="true"><Skeleton class="h-9" /><Skeleton class="h-9" /></div>
  {/if}
</SettingsSection>

<SettingsSheet
  bind:open={s3Open}
  title={m.artifact_s3_settings()}
  draft={() => [s3Draft, s3ExpiryText]}
  submitLabel={m.artifact_save()}
  saving={s3Saving}
  error={s3Error}
  onsubmit={saveS3}>
  <Field.FieldGroup>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-endpoint">{m.artifact_s3_endpoint()}</Field.FieldLabel>
      <Input id="artifact-s3-endpoint" bind:value={s3Draft.endpoint} disabled={s3Saving} />
    </Field.Field>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-region">{m.artifact_s3_region()}</Field.FieldLabel>
      <Input id="artifact-s3-region" bind:value={s3Draft.region} disabled={s3Saving} />
    </Field.Field>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-bucket">{m.artifact_s3_bucket()}</Field.FieldLabel>
      <Input id="artifact-s3-bucket" bind:value={s3Draft.bucket} disabled={s3Saving} />
    </Field.Field>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-access_key_id">{m.artifact_s3_access_key()}</Field.FieldLabel>
      <Input id="artifact-s3-access_key_id" bind:value={s3Draft.access_key_id} disabled={s3Saving} />
    </Field.Field>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-secret_access_key">{m.artifact_s3_secret_key()}</Field.FieldLabel>
      <SecretInput
        id="artifact-s3-secret_access_key"
        bind:value={s3Draft.secret_access_key}
        disabled={s3Saving}
        resetKey={s3Open} />
    </Field.Field>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-s3-session_token">{m.artifact_s3_session_token()}</Field.FieldLabel>
      <SecretInput
        id="artifact-s3-session_token"
        value={s3Draft.session_token ?? ''}
        oninput={(event: Event) => (s3Draft.session_token = (event.currentTarget as HTMLInputElement).value || null)}
        disabled={s3Saving}
        resetKey={s3Open} />
    </Field.Field>
    <Field.Field size="datetime">
      <Field.FieldLabel for="artifact-s3-expiry">{m.artifact_s3_expiry()}</Field.FieldLabel>
      <Field.FieldDescription>{m.artifact_s3_expiry_help()}</Field.FieldDescription>
      <Input id="artifact-s3-expiry" type="datetime-local" bind:value={s3ExpiryText} disabled={s3Saving} />
    </Field.Field>
  </Field.FieldGroup>
</SettingsSheet>

<SettingsSheet
  bind:open={downloadsOpen}
  title={m.artifact_external_downloads()}
  draft={() => publicUrlDraft}
  submitLabel={artifact?.external_signed_downloads ? m.artifact_save() : m.artifact_external_downloads_enable()}
  saving={downloadsSaving}
  submitDisabled={!publicUrlDraft.trim()}
  error={downloadsError}
  onsubmit={saveDownloads}>
  <Field.FieldGroup>
    <Field.Field size="fill">
      <Field.FieldLabel for="artifact-public-base-url">{m.artifact_public_base_url()}</Field.FieldLabel>
      <Input
        id="artifact-public-base-url"
        class="font-technical"
        bind:value={publicUrlDraft}
        disabled={downloadsSaving} />
      <Field.FieldDescription>
        {artifact?.s3 ? m.artifact_s3_warning() : m.artifact_public_warning()}
      </Field.FieldDescription>
    </Field.Field>
    <p class="text-sm text-muted-foreground">{m.artifact_transfer_warning()}</p>
  </Field.FieldGroup>
</SettingsSheet>
