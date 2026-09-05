<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import DownloadIcon from '@lucide/svelte/icons/download'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import { toast } from 'svelte-sonner'

import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'
import { openExternalUrl } from '$lib/open-external'
import { getProductUpdateCoordinator } from '$lib/product-update.svelte'

const updates = getProductUpdateCoordinator()
const available = $derived(updates.status?.available_update ?? null)
const checking = $derived(updates.state.phase === 'checking')
const busy = $derived(['downloading', 'installing'].includes(updates.state.phase))
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
    toast.error(
      m.settings_update_check_failed({
        message: updates.status?.last_failure?.message ?? updates.state.error ?? 'Unknown error',
      }),
    )
  }
}
</script>

<section id="updates" class="route-section scroll-mt-20 pb-8" aria-labelledby="updates-title">
  <div class="route-section-header">
    <div>
      <h2 id="updates-title" class="route-section-title">{m.settings_update()}</h2>
      <p class="route-section-description">{m.settings_update_summary()}</p>
    </div>
  </div>

  <div class="grid gap-4 rounded-xl border bg-card/40 p-4">
    <dl class="grid gap-3 text-sm sm:grid-cols-3">
      <div>
        <dt class="text-muted-foreground">{m.settings_update_current_version()}</dt>
        <dd class="font-technical mt-1 font-medium">{updates.status?.current_version ?? '–'}</dd>
      </div>
      <div>
        <dt class="text-muted-foreground">{m.settings_update_latest_version()}</dt>
        <dd class="font-technical mt-1 font-medium">{available?.version ?? '–'}</dd>
      </div>
      <div>
        <dt class="text-muted-foreground">{m.settings_update_last_checked()}</dt>
        <dd class="mt-1 font-medium">{lastChecked}</dd>
      </div>
    </dl>

    {#if updates.status?.last_failure}
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
          {#if progressPercent != null}<span class="font-technical">{progressPercent}%</span>{/if}
        </div>
        <div
          class="h-2 overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={progressPercent ?? undefined}>
          <div
            class={progressPercent == null
              ? 'h-full w-1/3 animate-pulse rounded-full bg-primary'
              : 'h-full rounded-full bg-primary transition-[width]'}
            style:width={progressPercent == null ? undefined : `${progressPercent}%`}></div>
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
    {:else if updates.state.phase === 'installing'}
      <p class="flex items-center gap-2 text-sm font-medium" aria-live="assertive">
        <Spinner />{m.settings_update_installing({ version: updates.state.targetVersion ?? '' })}
      </p>
    {:else if updates.state.phase === 'error' && updates.state.error}
      <p class="text-sm text-destructive" role="alert">{updates.state.error}</p>
    {/if}

    {#if !busy}
      <div class="flex flex-wrap gap-2">
        <Button variant="outline" disabled={checking} onclick={() => void checkNow()}>
          {#if checking}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon data-icon="inline-start" />{/if}
          {checking ? m.settings_update_checking() : m.settings_update_check_now()}
        </Button>
        {#if available}
          <Button variant="outline" onclick={() => void openExternalUrl(available.release_url)}>
            <ExternalLinkIcon data-icon="inline-start" />{m.settings_update_view_release()}
          </Button>
          {#if updates.status?.download_supported && available.download_available}
            {#if updates.state.phase === 'downloaded' && updates.state.targetVersion === available.version}
              <Button onclick={() => void updates.installDownloadedUpdate()}>{m.settings_update_install()}</Button>
            {:else}
              <Button onclick={() => void updates.downloadAvailableUpdate()}>
                <DownloadIcon data-icon="inline-start" />{updates.state.phase === 'error'
                  ? m.settings_update_retry_download()
                  : m.settings_update_download()}
              </Button>
            {/if}
          {/if}
          {#if updates.status?.skipped}
            <Button variant="ghost" onclick={() => void updates.clearSkippedVersion()}>
              {m.settings_update_restore_notifications()}
            </Button>
          {:else}
            <Button variant="ghost" onclick={() => void updates.skipAvailableVersion()}>
              {m.settings_update_skip()}
            </Button>
          {/if}
        {/if}
      </div>
    {/if}
  </div>
</section>
