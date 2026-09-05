<script lang="ts">
import { browser } from '$app/environment'
import * as m from '$lib/paraglide/messages.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import DownloadIcon from '@lucide/svelte/icons/download'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import { toast } from 'svelte-sonner'

import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'
import { openExternalUrl } from '$lib/open-external'
import { supportsInAppInstallProgress } from '$lib/product-update'
import { getProductUpdateCoordinator } from '$lib/product-update.svelte'

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

<section id="updates" class="route-section scroll-mt-20 pb-8" aria-labelledby="updates-title">
  <div class="route-section-header flex-wrap">
    <div>
      <h2 id="updates-title" class="route-section-title">{m.settings_update()}</h2>
      <p class="route-section-description">{m.settings_update_summary()}</p>
    </div>
    <Button
      variant="outline"
      disabled={checking || busy || checkUnavailable}
      aria-describedby={checkUnavailable ? 'update-check-unavailable' : undefined}
      onclick={() => void checkNow()}>
      {#if checking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon data-icon="inline-start" />{/if}
      {checking ? m.settings_update_checking() : m.settings_update_check_now()}
    </Button>
  </div>

  <div class="flex flex-col gap-5">
    <dl class="flex flex-wrap items-start gap-x-10 gap-y-4 text-sm">
      <div class="min-w-0">
        <dt class="text-muted-foreground">{m.settings_update_current_version()}</dt>
        <dd class="font-technical mt-2 flex items-center gap-10 text-2xl leading-tight font-medium tabular-nums">
          <span class="break-all">{updates.status?.current_version ?? '–'}</span>
          {#if available}<ArrowRightIcon class="size-5 shrink-0 text-muted-foreground" aria-hidden="true" />{/if}
        </dd>
      </div>
      {#if available}
        <div class="min-w-0">
          <dt class="text-muted-foreground">{m.settings_update_latest_version()}</dt>
          <dd class="font-technical mt-2 text-2xl leading-tight font-medium break-all text-primary tabular-nums">
            {available.version}
          </dd>
        </div>
      {/if}
    </dl>

    {#if checkUnavailable}
      <p id="update-check-unavailable" class="text-sm text-pretty text-muted-foreground" role="status">
        {m.settings_update_check_unavailable()}
      </p>
    {:else if updates.status?.last_failure}
      <p class="text-sm text-destructive" role="alert">
        {m.settings_update_check_failed({ message: updates.status.last_failure.message })}
      </p>
    {/if}

    {#if available}
      <div class="grid gap-2">
        <p class="text-sm font-medium">{m.settings_update_available({ version: available.version })}</p>
        {#if updates.status?.skipped}
          <p class="text-sm text-muted-foreground">{m.settings_update_skipped()}</p>
        {/if}
        {#if available.download_error}
          <p class="text-sm text-destructive" role="alert">{available.download_error}</p>
        {/if}
      </div>
    {:else if updates.status?.check_status === 'up-to-date'}
      <p class="text-sm text-muted-foreground">{m.settings_update_up_to_date()}</p>
    {/if}

    {#if updates.state.phase === 'downloading'}
      <div class="grid gap-2" aria-live="polite">
        <div class="flex items-center justify-between gap-3 text-sm">
          <span>{m.settings_update_downloading({ version: updates.state.targetVersion ?? '' })}</span>
          {#if progressPercent != null}<span class="font-technical tabular-nums">{progressPercent}%</span>{/if}
        </div>
        <div
          class="h-2 overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-label={m.settings_update_downloading({ version: updates.state.targetVersion ?? '' })}
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={progressPercent ?? undefined}>
          <div
            class={progressPercent == null
              ? 'h-full w-1/3 rounded-full bg-primary motion-safe:animate-pulse'
              : 'h-full rounded-full bg-primary motion-safe:transition-[width]'}
            style:width={progressPercent == null ? undefined : `${progressPercent}%`}>
          </div>
        </div>
      </div>
    {:else if updates.state.phase === 'downloaded'}
      <p class="text-sm font-medium">
        {m.settings_update_downloaded({ version: updates.state.targetVersion ?? '' })}
      </p>
      {#if updates.state.error}
        <p class="text-sm text-destructive" role="alert">
          {m.settings_update_install_failed({ message: updates.state.error })}
        </p>
      {/if}
    {:else if updates.state.phase === 'installing' && showInstallingProgress}
      <p class="flex items-center gap-2 text-sm font-medium" aria-live="assertive">
        <Spinner />{m.settings_update_installing({ version: updates.state.targetVersion ?? '' })}
      </p>
    {:else if updates.state.phase === 'error' && updates.state.error}
      <p class="text-sm text-destructive" role="alert">{updates.state.error}</p>
    {/if}

    <div class="flex flex-wrap items-end justify-between gap-x-6 gap-y-4 border-t pt-4">
      <dl class="flex flex-wrap gap-x-2 gap-y-1 text-xs text-muted-foreground">
        <dt>{m.settings_update_last_checked()}</dt>
        <dd class="tabular-nums">{lastChecked}</dd>
      </dl>
      {#if !busy}
        <div class="flex flex-wrap items-center gap-2">
          {#if available}
            {#if updates.status?.skipped}
              <Button variant="ghost" onclick={() => void updates.clearSkippedVersion()}>
                {m.settings_update_restore_notifications()}
              </Button>
            {:else}
              <Button variant="ghost" onclick={() => void updates.skipAvailableVersion()}>
                {m.settings_update_skip()}
              </Button>
            {/if}
            <Button variant="outline" onclick={() => void openExternalUrl(available.release_url)}>
              <ExternalLinkIcon data-icon="inline-start" />{m.settings_update_view_release()}
            </Button>
          {/if}
          {#if updates.state.phase === 'downloaded'}
            <Button onclick={() => updates.requestInstallPrompt()}>{m.settings_update_install()}</Button>
          {/if}
          {#if available && updates.status?.download_supported && available.download_available && (updates.state.phase !== 'downloaded' || updates.state.targetVersion !== available.version)}
            <Button onclick={() => void updates.downloadAvailableUpdate()}>
              <DownloadIcon data-icon="inline-start" />{updates.state.phase === 'error'
                ? m.settings_update_retry_download()
                : m.settings_update_download()}
            </Button>
          {/if}
        </div>
      {/if}
    </div>
  </div>
</section>
