<script lang="ts">
import { browser } from '$app/env'
import { resolve } from '$app/paths'
import * as m from '#lib/paraglide/messages.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import CircleArrowUpIcon from '@lucide/svelte/icons/circle-arrow-up'
import CircleCheckIcon from '@lucide/svelte/icons/circle-check'
import DownloadIcon from '@lucide/svelte/icons/download'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert'
import { toast } from 'svelte-sonner'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import { Switch } from '#lib/components/ui/switch/index.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Progress } from '#lib/components/ui/progress/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { openExternalUrl } from '#lib/open-external.js'
import { supportsInAppInstallProgress } from '#lib/product-update.js'
import { getProductUpdateCoordinator } from '#lib/product-update.svelte.js'
import { settingsGroupHref } from '#lib/settings-groups.js'

const queryClient = useQueryClient()

const proxyPreferenceQuery = createQuery(() => ({
  queryKey: ['setting', 'update_use_proxy'],
  queryFn: () => admin.settings.get('update_use_proxy'),
}))

const useProxy = $derived(proxyPreferenceQuery.data === 'true')
let savingProxy = $state(false)
let proxyError = $state('')
let proxyNeedsConfiguration = $state(false)
let retryProxyValue = $state<boolean>()

async function toggleProxy(value: boolean): Promise<void> {
  if (savingProxy || proxyPreferenceQuery.data === undefined) return
  savingProxy = true
  proxyError = ''
  proxyNeedsConfiguration = false
  retryProxyValue = value
  try {
    if (value) {
      const saved = await admin.settings.proxy()
      if (!saved.url.trim()) {
        proxyNeedsConfiguration = true
        return
      }
    }
    await admin.settings.set('update_use_proxy', value ? 'true' : 'false')
    queryClient.setQueryData(['setting', 'update_use_proxy'], value ? 'true' : 'false')
    retryProxyValue = undefined
  } catch (error) {
    proxyError = localizeBackendErrorMessage(error)
  } finally {
    savingProxy = false
  }
}

const updates = getProductUpdateCoordinator()
const available = $derived(updates.status?.available_update ?? null)
const checking = $derived(updates.state.phase === 'checking')
const checkUnavailable = $derived(updates.status?.last_failure?.code === 'UPDATE_CHECK_DISABLED')
const busy = $derived(['downloading', 'installing'].includes(updates.state.phase))
const showInstallingProgress = $derived(supportsInAppInstallProgress(browser ? navigator.userAgent : ''))
const progressPercent = $derived.by(() => {
  const total = updates.state.totalBytes

  if (!total || total <= 0) return null

  return Math.min(100, Math.round((updates.state.downloadedBytes / total) * 100))
})

const lastChecked = $derived(
  updates.status?.last_success_at
    ? new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(
        new Date(updates.status.last_success_at),
      )
    : m.settings_update_never_checked(),
)

async function checkNow(): Promise<void> {
  const result = await updates.manualCheck()
  if (result === 'up-to-date') toast.success(m.settings_update_up_to_date())
  else if (result === 'available' && updates.status?.available_update) {
    toast.success(m.settings_update_available({ version: updates.status.available_update.version }))
  } else if (result === 'error') {
    if (checkUnavailable) {
      toast.info(m.settings_update_check_unavailable())
    } else {
      toast.error(
        m.settings_update_check_failed({
          message: updates.state.error ?? updates.status?.last_failure?.message ?? 'Unknown error',
        }),
      )
    }
  }
}
</script>

{#snippet checkButton()}
  <Button
    type="button"
    variant="outline"
    size="sm"
    disabled={checking || busy || checkUnavailable}
    aria-describedby={checkUnavailable ? 'update-check-unavailable' : undefined}
    onclick={() => void checkNow()}>
    {#if checking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon data-icon="inline-start" />{/if}
    {checking ? m.settings_update_checking() : m.settings_update_check_now()}
  </Button>
{/snippet}

<SettingsSection id="updates" title={m.settings_update()}>
  <SettingRow title={m.settings_version()}>
    <div
      class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2 @min-[50rem]/setting-row:max-w-[36rem] @min-[50rem]/setting-row:justify-end">
      <span class="flex items-center gap-2 font-technical text-sm tabular-nums">
        <span class="break-all">{updates.status?.current_version ?? '–'}</span>
        {#if available}
          <ArrowRightIcon class="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          <span class="break-all text-primary">{available.version}</span>
        {/if}
      </span>
      {#if available}
        <span class="inline-flex items-center gap-1.5 text-sm font-medium text-primary">
          <CircleArrowUpIcon class="size-4" aria-hidden="true" />{m.settings_update_status_available()}
        </span>
      {:else if updates.status?.last_failure && !checkUnavailable}
        <span class="inline-flex items-center gap-1.5 text-sm font-medium text-destructive">
          <TriangleAlertIcon class="size-4" aria-hidden="true" />{m.settings_update_status_failed()}
        </span>
      {:else if updates.status?.check_status === 'up-to-date'}
        <span class="inline-flex items-center gap-1.5 text-sm text-muted-foreground">
          <CircleCheckIcon class="size-4" aria-hidden="true" />{m.settings_update_status_current()}
        </span>
      {/if}
      {@render checkButton()}
      {#if !busy}
        {#if available}
          {#if updates.status?.skipped}
            <Button type="button" variant="ghost" size="sm" onclick={() => void updates.clearSkippedVersion()}>
              {m.settings_update_restore_notifications()}
            </Button>
          {:else}
            <Button type="button" variant="ghost" size="sm" onclick={() => void updates.skipAvailableVersion()}>
              {m.settings_update_skip()}
            </Button>
          {/if}
          <Button type="button" variant="outline" size="sm" onclick={() => void openExternalUrl(available.release_url)}>
            <ExternalLinkIcon data-icon="inline-start" />{m.settings_update_view_release()}
          </Button>
        {/if}
        {#if updates.state.phase === 'downloaded'}
          <Button type="button" size="sm" onclick={() => updates.requestInstallPrompt()}>
            {m.settings_update_install()}
          </Button>
        {/if}
        {#if available && updates.status?.download_supported && available.download_available && (updates.state.phase !== 'downloaded' || updates.state.targetVersion !== available.version)}
          <Button type="button" size="sm" onclick={() => void updates.downloadAvailableUpdate()}>
            <DownloadIcon data-icon="inline-start" />{updates.state.phase === 'error'
              ? m.settings_update_retry_download()
              : m.settings_update_download()}
          </Button>
        {/if}
      {/if}
    </div>
  </SettingRow>

  <div class="flex flex-col gap-3 pb-3">
    {#if updates.status?.skipped && available}
      <p role="status" class="text-sm text-muted-foreground">{m.settings_update_skipped()}</p>
    {/if}
    {#if !updates.status?.last_success_at || (updates.status?.last_failure && !checkUnavailable)}
      <p class="text-xs text-muted-foreground">
        {m.settings_update_last_checked()} <span class="tabular-nums">{lastChecked}</span>
      </p>
    {/if}

    {#if checkUnavailable}
      <Alert.Root id="update-check-unavailable" role="status">
        <Alert.Description>{m.settings_update_check_unavailable()}</Alert.Description>
      </Alert.Root>
    {:else if updates.status?.last_failure}
      <Alert.Root variant="destructive">
        <Alert.Description
          >{m.settings_update_check_failed({ message: updates.status.last_failure.message })}</Alert.Description>
      </Alert.Root>
    {/if}

    {#if available?.download_error}
      <Alert.Root variant="destructive">
        <Alert.Description>{available.download_error}</Alert.Description>
      </Alert.Root>
    {/if}

    {#if updates.state.phase === 'downloading'}
      <div class="grid gap-2" aria-live="polite">
        <div class="flex items-center justify-between gap-3 text-sm">
          <span>{m.settings_update_downloading({ version: updates.state.targetVersion ?? '' })}</span>
          {#if progressPercent != null}<span class="font-technical tabular-nums">{progressPercent}%</span>{/if}
        </div>
        <Progress
          value={progressPercent}
          aria-label={m.settings_update_downloading({ version: updates.state.targetVersion ?? '' })} />
      </div>
    {:else if updates.state.phase === 'downloaded'}
      <Alert.Root role="status">
        <Alert.Description
          >{m.settings_update_downloaded({ version: updates.state.targetVersion ?? '' })}</Alert.Description>
      </Alert.Root>
      {#if updates.state.error}
        <Alert.Root variant="destructive">
          <Alert.Description>{m.settings_update_install_failed({ message: updates.state.error })}</Alert.Description>
        </Alert.Root>
      {/if}
    {:else if updates.state.phase === 'installing' && showInstallingProgress}
      <Alert.Root role="status" aria-live="assertive">
        <Spinner />
        <Alert.Description
          >{m.settings_update_installing({ version: updates.state.targetVersion ?? '' })}</Alert.Description>
      </Alert.Root>
    {:else if updates.state.phase === 'error' && updates.state.error}
      <Alert.Root variant="destructive">
        <Alert.Description>{updates.state.error}</Alert.Description>
      </Alert.Root>
    {/if}
  </div>

  {#if proxyPreferenceQuery.error}
    <RequestFailure
      title={m.settings_update_proxy_load_failed()}
      message={localizeBackendErrorMessage(proxyPreferenceQuery.error)}
      retry={() => void proxyPreferenceQuery.refetch()}
      retrying={proxyPreferenceQuery.isFetching} />
  {:else if proxyPreferenceQuery.isPending}
    <p role="status" class="flex items-center gap-2 border-t py-3 text-sm text-muted-foreground">
      <Spinner />{m.common_loading()}
    </p>
  {:else}
    <SettingRow layout="inline" title={m.settings_update_use_proxy()} labelFor="update-use-proxy">
      {#if savingProxy}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="update-use-proxy"
        bind:checked={() => useProxy, (value: boolean) => void toggleProxy(value)}
        disabled={savingProxy} />
    </SettingRow>
  {/if}
  {#if proxyNeedsConfiguration}
    <p role="alert" class="pb-3 text-sm">
      <a href={resolve(settingsGroupHref('network', 'proxy'))} class="text-primary underline underline-offset-4"
        >{m.settings_update_proxy_configure_first()}</a>
    </p>
  {/if}
  {#if proxyError}
    <RequestFailure
      title={m.settings_update_proxy_save_failed()}
      message={proxyError}
      retry={() => {
        if (retryProxyValue !== undefined) void toggleProxy(retryProxyValue)
      }}
      retrying={savingProxy} />
  {/if}
</SettingsSection>
