<script lang="ts">
import AlertTriangleIcon from '@lucide/svelte/icons/alert-triangle'
import CopyIcon from '@lucide/svelte/icons/copy'
import FileTextIcon from '@lucide/svelte/icons/file-text'
import LogOutIcon from '@lucide/svelte/icons/log-out'
import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw'
import { userPrefersMode } from 'mode-watcher'
import { onMount } from 'svelte'

import BrandMark from '$lib/components/brand-mark.svelte'
import BrandWordmark from '$lib/components/brand-wordmark.svelte'
import WindowControls from '$lib/components/window-controls.svelte'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'
import { exitDesktop, openDesktopLogs, restartDesktop } from '$lib/desktop-startup'
import type { DesktopStartupStage, DesktopStartupState } from '$lib/desktop-startup'
import * as m from '$lib/paraglide/messages.js'
import { createWindowChrome } from '$lib/window-chrome'

let { state: startup }: { state: DesktopStartupState } = $props()

const windowChrome = createWindowChrome()
let isWindowMaximized = $state(false)
let pendingAction = $state<'restart' | 'exit' | 'logs' | null>(null)
let feedback = $state.raw<{ message: string; error: boolean } | null>(null)

const stage = $derived(stageLabel(startup.stage))
const technicalDiagnostics = $derived.by(() => {
  const diagnostics = [
    `${m.desktop_startup_technical_stage()}: ${stage}`,
    `${m.desktop_startup_technical_code()}: ${startup.error?.code ?? startup.stage}`,
    `${m.desktop_startup_technical_details()}: ${startup.error?.details || m.desktop_startup_technical_unavailable()}`,
  ]
  if (startup.logPath) diagnostics.push(`${m.desktop_startup_technical_log()}: ${startup.logPath}`)
  return diagnostics.join('\n')
})

$effect(() => {
  const themePreference = userPrefersMode.current
  windowChrome.syncTheme(themePreference === 'system' ? null : themePreference)
})

function stageLabel(value: DesktopStartupStage): string {
  switch (value) {
    case 'data_directory':
      return m.desktop_startup_stage_data_directory()
    case 'gateway':
      return m.desktop_startup_stage_gateway()
    case 'session':
      return m.desktop_startup_stage_session()
    case 'http':
      return m.desktop_startup_stage_http()
    case 'desktop':
      return m.desktop_startup_stage_desktop()
  }
}

function failureImpact(value: DesktopStartupStage): string {
  switch (value) {
    case 'data_directory':
      return m.desktop_startup_failure_data_directory()
    case 'gateway':
      return m.desktop_startup_failure_gateway()
    case 'session':
      return m.desktop_startup_failure_session()
    case 'http':
      return m.desktop_startup_failure_http()
    case 'desktop':
      return m.desktop_startup_failure_desktop()
  }
}

async function copyDiagnostics(): Promise<void> {
  try {
    await navigator.clipboard.writeText(technicalDiagnostics)
    feedback = { message: m.desktop_startup_diagnostics_copied(), error: false }
  } catch {
    feedback = { message: m.desktop_startup_diagnostics_copy_failed(), error: true }
  }
}

async function runAction(action: 'restart' | 'exit' | 'logs'): Promise<void> {
  pendingAction = action
  feedback = null
  try {
    if (action === 'restart') await restartDesktop()
    else if (action === 'exit') await exitDesktop()
    else await openDesktopLogs()
  } catch {
    feedback = {
      message:
        action === 'restart'
          ? m.desktop_startup_restart_failed()
          : action === 'exit'
            ? m.desktop_startup_exit_failed()
            : m.desktop_startup_open_logs_failed(),
      error: true,
    }
  } finally {
    pendingAction = null
  }
}

onMount(() => {
  let disposed = false
  let stopWindowObservation: (() => void) | undefined
  if (windowChrome.controls === 'custom') {
    void windowChrome
      .observeMaximized((maximized) => (isWindowMaximized = maximized))
      .then((stop) => {
        if (disposed) stop()
        else stopWindowObservation = stop
      })
  }

  return () => {
    disposed = true
    stopWindowObservation?.()
  }
})
</script>

<div
  class="flex h-svh min-h-screen flex-col overflow-hidden bg-background text-foreground"
  data-testid="desktop-startup"
  data-status={startup.status}>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <header
    class="flex h-10 shrink-0 select-none items-stretch bg-sidebar"
    onmousedown={(event) => windowChrome.startDrag(event)}
    ondblclick={(event) => windowChrome.toggleMaximize(event)}>
    <div
      class={['flex min-w-0 items-center gap-2 px-2', windowChrome.controls === 'native' ? 'ps-20' : '']}
      aria-label="Stravia 观策行">
      <BrandMark class="size-6" state={startup.status === 'starting' ? 'running' : 'static'} />
      <BrandWordmark class="text-sm" />
    </div>
    <div class="min-w-0 flex-1"></div>
    {#if windowChrome.controls === 'custom'}
      <WindowControls
        isMaximized={isWindowMaximized}
        minimizeLabel={m.app_shell_minimize_window()}
        maximizeLabel={m.app_shell_maximize_window()}
        restoreLabel={m.app_shell_restore_window()}
        closeLabel={m.app_shell_close_window()}
        onMinimize={windowChrome.minimize}
        onToggleMaximize={() => windowChrome.toggleMaximize()}
        onClose={() => void runAction('exit')} />
    {/if}
  </header>

  <main class="grid min-h-0 min-w-0 flex-1 grid-cols-1 overflow-y-auto p-4 sm:p-8">
    <section class="m-auto flex w-full min-w-0 max-w-2xl flex-col gap-6" aria-labelledby="desktop-startup-title">
      {#if startup.status === 'starting'}
        <div class="flex items-start gap-4" role="status" aria-live="polite">
          <div class="flex size-10 shrink-0 items-center justify-center rounded-lg border bg-card">
            <Spinner class="size-5" />
          </div>
          <div class="min-w-0">
            <p class="font-structural text-xs font-semibold text-muted-foreground">
              {m.desktop_startup_stage_label()}
            </p>
            <h1 id="desktop-startup-title" class="font-structural mt-1 text-2xl font-semibold text-balance">
              {m.desktop_startup_starting_title()}
            </h1>
            <p class="mt-2 text-base font-medium">{stage}</p>
            <p class="mt-1 text-sm text-muted-foreground text-pretty">
              {m.desktop_startup_starting_description()}
            </p>
          </div>
        </div>
      {:else}
        <div class="flex items-start gap-4" role="alert">
          <div
            class="flex size-10 shrink-0 items-center justify-center rounded-lg border border-destructive/40 text-destructive">
            <AlertTriangleIcon aria-hidden="true" />
          </div>
          <div class="min-w-0">
            <p class="font-structural text-xs font-semibold text-destructive">{stage}</p>
            <h1 id="desktop-startup-title" class="font-structural mt-1 text-2xl font-semibold text-balance">
              {m.desktop_startup_failed_title()}
            </h1>
            <p class="mt-2 text-sm text-pretty">{failureImpact(startup.stage)}</p>
            <p class="mt-2 text-sm text-muted-foreground text-pretty">
              {m.desktop_startup_failed_action()}
            </p>
          </div>
        </div>

        <details class="rounded-lg border bg-card px-4 py-3">
          <summary class="min-h-10 cursor-pointer py-2 font-medium">
            {m.desktop_startup_technical_summary()}
          </summary>
          <pre
            class="mt-3 overflow-x-auto border-t pt-3 font-mono text-xs whitespace-pre-wrap text-muted-foreground">{technicalDiagnostics}</pre>
          <div class="mt-3 flex flex-wrap gap-2">
            <Button data-testid="startup-copy" variant="outline" size="sm" onclick={copyDiagnostics}>
              <CopyIcon data-icon="inline-start" />
              {m.desktop_startup_copy_diagnostics()}
            </Button>
            {#if startup.logPath}
              <Button
                data-testid="startup-logs"
                variant="outline"
                size="sm"
                disabled={pendingAction !== null}
                aria-busy={pendingAction === 'logs'}
                onclick={() => void runAction('logs')}>
                {#if pendingAction === 'logs'}
                  <Spinner data-icon="inline-start" />
                {:else}
                  <FileTextIcon data-icon="inline-start" />
                {/if}
                {m.desktop_startup_open_logs()}
              </Button>
            {/if}
          </div>
        </details>

        <div class="flex flex-col gap-2 sm:flex-row sm:justify-end">
          <Button
            data-testid="startup-exit"
            variant="outline"
            disabled={pendingAction !== null}
            aria-busy={pendingAction === 'exit'}
            onclick={() => void runAction('exit')}>
            {#if pendingAction === 'exit'}
              <Spinner data-icon="inline-start" />
            {:else}
              <LogOutIcon data-icon="inline-start" />
            {/if}
            {m.desktop_startup_exit()}
          </Button>
          <Button
            data-testid="startup-restart"
            disabled={pendingAction !== null}
            aria-busy={pendingAction === 'restart'}
            onclick={() => void runAction('restart')}>
            {#if pendingAction === 'restart'}
              <Spinner data-icon="inline-start" />
            {:else}
              <RotateCcwIcon data-icon="inline-start" />
            {/if}
            {m.desktop_startup_restart()}
          </Button>
        </div>
      {/if}

      {#if feedback}
        <p
          class={['text-sm', feedback.error ? 'text-destructive' : 'text-signal']}
          role={feedback.error ? 'alert' : 'status'}
          aria-live="polite">
          {feedback.message}
        </p>
      {/if}
    </section>
  </main>
</div>
