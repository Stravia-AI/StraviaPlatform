<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import DownloadIcon from '@lucide/svelte/icons/download'
import { listen } from '@tauri-apps/api/event'
import { toast } from 'svelte-sonner'
import { onDestroy } from 'svelte'

import { admin, isTauri } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatBytes } from '#lib/format.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

const queryClient = useQueryClient()
const debugQuery = createQuery(() => ({ queryKey: ['observation-debug'], queryFn: admin.observations.debug }))
let debugConfirmOpen = $state(false)
let debugClearOpen = $state(false)
let changingDebug = $state(false)
let clearingDebug = $state(false)
let downloadingPerformance = $state<'metrics' | 'timeline' | null>(null)
let performanceDownloadHref: string | undefined
let performanceDownloadDisposed = false
let performanceDownloadUnlisten: (() => void) | undefined
let cancelPerformanceDownload: (() => void) | undefined

interface DesktopDownloadFinished {
  url: string
  path: string | null
  success: boolean
}

onDestroy(() => {
  performanceDownloadDisposed = true
  cancelPerformanceDownload?.()
  performanceDownloadUnlisten?.()
  performanceDownloadUnlisten = undefined
  if (performanceDownloadHref) URL.revokeObjectURL(performanceDownloadHref)
  performanceDownloadHref = undefined
})

async function setDebug(enabled: boolean): Promise<void> {
  if (changingDebug || debugQuery.isFetching || debugQuery.error || !debugQuery.data) return
  changingDebug = true
  try {
    const confirmed = await admin.observations.setDebug(enabled)
    queryClient.setQueryData(['observation-debug'], confirmed)
    debugConfirmOpen = false
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    changingDebug = false
  }
}

async function clearDebugData(): Promise<void> {
  if (clearingDebug || debugQuery.isFetching || debugQuery.error || !debugQuery.data) return
  clearingDebug = true
  try {
    const confirmed = await admin.observations.clearDebug()
    queryClient.setQueryData(['observation-debug'], confirmed)
    debugClearOpen = false
    toast.success(m.observation_debug_cleared())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    clearingDebug = false
  }
}

async function downloadPerformance(kind: 'metrics' | 'timeline'): Promise<void> {
  if (downloadingPerformance) return
  downloadingPerformance = kind
  try {
    const blob = await admin.performance[kind]()
    if (performanceDownloadDisposed) return
    if (performanceDownloadHref) URL.revokeObjectURL(performanceDownloadHref)
    const href = URL.createObjectURL(blob)
    performanceDownloadHref = href
    let completed: PromiseWithResolvers<DesktopDownloadFinished | null> | undefined
    if (isTauri) {
      completed = Promise.withResolvers<DesktopDownloadFinished | null>()
      cancelPerformanceDownload = () => completed?.resolve(null)
      performanceDownloadUnlisten = await listen<DesktopDownloadFinished>('desktop-download-finished', (event) => {
        if (event.payload.url === href) {
          cancelPerformanceDownload = undefined
          // The native download completion is the only reliable success signal.
          completed?.resolve(event.payload)
        }
      })
      if (performanceDownloadDisposed) return
    }
    const link = document.createElement('a')
    link.href = href
    link.download = kind === 'metrics' ? 'stravia-performance-metrics.prom' : 'stravia-performance-trace.json'
    document.body.append(link)
    try {
      link.click()
    } catch (error) {
      if (!isTauri) {
        URL.revokeObjectURL(href)
        performanceDownloadHref = undefined
      }
      throw error
    } finally {
      link.remove()
    }
    if (completed) {
      const result = await completed.promise
      if (!performanceDownloadDisposed && result) {
        if (result.success) {
          toast.success(
            result.path
              ? m.settings_diagnostics_download_saved_to({ path: result.path })
              : m.settings_diagnostics_download_completed(),
          )
        } else {
          toast.error(m.settings_diagnostics_download_failed())
        }
      }
    }
  } catch (error) {
    if (!performanceDownloadDisposed) toast.error(localizeBackendErrorMessage(error))
  } finally {
    performanceDownloadUnlisten?.()
    performanceDownloadUnlisten = undefined
    cancelPerformanceDownload = undefined
    if (isTauri && performanceDownloadHref) {
      URL.revokeObjectURL(performanceDownloadHref)
      performanceDownloadHref = undefined
    }
    downloadingPerformance = null
  }
}
</script>

<SettingsSection id="diagnostics" title={m.settings_diagnostics()}>
  {#if debugQuery.error}
    <RequestFailure
      title={m.settings_diagnostics_load_failed()}
      message={localizeBackendErrorMessage(debugQuery.error)}
      retry={() => void debugQuery.refetch()}
      retrying={debugQuery.isFetching} />
  {:else if debugQuery.data}
    <SettingRow
      layout="inline"
      title={m.settings_diagnostics_debug()}
      labelFor="diagnostics-debug"
      description={debugQuery.data.enabled ? m.settings_diagnostics_export_before_off() : undefined}
      value={debugQuery.data.retained_bytes > 0 ? formatBytes(debugQuery.data.retained_bytes) : undefined}>
      {#if changingDebug}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="diagnostics-debug"
        bind:checked={
          () => debugQuery.data?.enabled ?? false,
          (enabled) => (enabled ? (debugConfirmOpen = true) : void setDebug(false))
        }
        disabled={changingDebug || clearingDebug || debugQuery.isFetching}
        aria-busy={changingDebug} />
    </SettingRow>
    <SettingRow title={m.settings_performance_data()}>
      <div class="flex flex-wrap justify-end gap-2">
        <Button
          variant="outline"
          size="sm"
          disabled={downloadingPerformance !== null}
          aria-busy={downloadingPerformance === 'metrics'}
          onclick={() => void downloadPerformance('metrics')}>
          {#if downloadingPerformance === 'metrics'}<Spinner data-icon="inline-start" />{:else}<DownloadIcon
              data-icon="inline-start" />{/if}
          {m.settings_diagnostics_download_metrics()}
        </Button>
        <Button
          variant="outline"
          size="sm"
          disabled={downloadingPerformance !== null}
          aria-busy={downloadingPerformance === 'timeline'}
          onclick={() => void downloadPerformance('timeline')}>
          {#if downloadingPerformance === 'timeline'}<Spinner data-icon="inline-start" />{:else}<DownloadIcon
              data-icon="inline-start" />{/if}
          {m.settings_diagnostics_download_timeline()}
        </Button>
      </div>
    </SettingRow>
    {#if debugQuery.data.enabled || debugQuery.data.retained_bytes > 0}
      <SettingRow title={m.settings_debug_data()}>
        <Button
          variant="destructive"
          size="sm"
          disabled={clearingDebug || changingDebug || debugQuery.isFetching}
          onclick={() => (debugClearOpen = true)}>{m.observation_clear_debug()}</Button>
      </SettingRow>
    {/if}
  {:else}
    <Skeleton class="my-3 h-9" aria-busy="true" />
  {/if}
</SettingsSection>

<AlertDialog.Root bind:open={debugConfirmOpen}>
  {#if debugQuery.data && !debugQuery.error}<AlertDialog.Content>
      <AlertDialog.Header>
        <AlertDialog.Title>{m.observation_enable_debug()}</AlertDialog.Title>
        <AlertDialog.Description>
          {m.observation_debug_warning({ retention_days: debugQuery.data.retention_days })}
        </AlertDialog.Description>
      </AlertDialog.Header>
      <AlertDialog.Footer>
        <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
        <AlertDialog.Action disabled={changingDebug || debugQuery.isFetching} onclick={() => void setDebug(true)}>
          {changingDebug ? m.observation_enabling() : m.observation_enable_debug()}
        </AlertDialog.Action>
      </AlertDialog.Footer>
    </AlertDialog.Content>{/if}
</AlertDialog.Root>

<AlertDialog.Root bind:open={debugClearOpen}>
  {#if debugQuery.data && !debugQuery.error}<AlertDialog.Content>
      <AlertDialog.Header>
        <AlertDialog.Title>{m.observation_clear_debug()}</AlertDialog.Title>
        <AlertDialog.Description>
          {m.observation_clear_debug_warning({ retained: formatBytes(debugQuery.data.retained_bytes) })}
        </AlertDialog.Description>
      </AlertDialog.Header>
      <AlertDialog.Footer>
        <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
        <AlertDialog.Action
          variant="destructive"
          disabled={clearingDebug || debugQuery.isFetching}
          onclick={() => void clearDebugData()}>
          {clearingDebug ? m.observation_clearing() : m.observation_clear_debug()}
        </AlertDialog.Action>
      </AlertDialog.Footer>
    </AlertDialog.Content>{/if}
</AlertDialog.Root>
