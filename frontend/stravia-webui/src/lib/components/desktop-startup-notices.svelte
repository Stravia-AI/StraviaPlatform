<script lang="ts">
import AlertTriangleIcon from '@lucide/svelte/icons/alert-triangle'
import FileTextIcon from '@lucide/svelte/icons/file-text'
import XIcon from '@lucide/svelte/icons/x'

import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'
import { openDesktopLogs } from '$lib/desktop-startup'
import type { DesktopStartupState, DesktopStartupWarning } from '$lib/desktop-startup'
import * as m from '$lib/paraglide/messages.js'

let { state: startup }: { state: DesktopStartupState } = $props()

let dismissed = $state<string[]>([])
let openingLogs = $state(false)
let openLogsFailed = $state(false)

const visibleWarnings = $derived(
  startup.warnings
    .map((warning, index) => ({ id: `${warning.code}-${index}`, warning }))
    .filter(({ id }) => !dismissed.includes(id)),
)
const showPreviousFailure = $derived(startup.previousFailure && !dismissed.includes('previous-failure'))

function warningLabel(warning: DesktopStartupWarning): string {
  switch (warning.code) {
    case 'autostart':
      return m.desktop_startup_warning_autostart()
    case 'tray':
      return m.desktop_startup_warning_tray()
    case 'icons':
      return m.desktop_startup_warning_icons()
    case 'diagnostics':
      return m.desktop_startup_warning_diagnostics()
    default:
      return m.desktop_startup_warning_unknown()
  }
}

function dismiss(id: string): void {
  dismissed = [...dismissed, id]
}

async function openLogs(): Promise<void> {
  openingLogs = true
  openLogsFailed = false
  try {
    await openDesktopLogs()
  } catch {
    openLogsFailed = true
  } finally {
    openingLogs = false
  }
}
</script>

{#if showPreviousFailure || visibleWarnings.length > 0}
  <section class="mb-6 flex flex-col gap-3" aria-label={m.desktop_startup_notices_label()}>
    {#if showPreviousFailure}
      <Alert.Root variant="warning">
        <AlertTriangleIcon />
        <Alert.Title>{m.desktop_startup_previous_failure_title()}</Alert.Title>
        <Alert.Description>
          <p>{m.desktop_startup_previous_failure_description()}</p>
          {#if startup.logPath}
            <Button
              class="mt-3"
              variant="outline"
              size="sm"
              disabled={openingLogs}
              aria-busy={openingLogs}
              onclick={() => void openLogs()}>
              {#if openingLogs}
                <Spinner data-icon="inline-start" />
              {:else}
                <FileTextIcon data-icon="inline-start" />
              {/if}
              {m.desktop_startup_open_logs()}
            </Button>
          {/if}
        </Alert.Description>
        <Alert.Action>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={m.desktop_startup_dismiss_notice()}
            title={m.desktop_startup_dismiss_notice()}
            onclick={() => dismiss('previous-failure')}>
            <XIcon />
          </Button>
        </Alert.Action>
      </Alert.Root>
    {/if}

    {#each visibleWarnings as item (item.id)}
      <Alert.Root variant="warning">
        <AlertTriangleIcon />
        <Alert.Title>{m.desktop_startup_warning_title()}</Alert.Title>
        <Alert.Description>
          <p>{warningLabel(item.warning)}</p>
          {#if item.warning.details}
            <p class="mt-1 font-mono text-xs">{item.warning.details}</p>
          {/if}
        </Alert.Description>
        <Alert.Action>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={m.desktop_startup_dismiss_notice()}
            title={m.desktop_startup_dismiss_notice()}
            onclick={() => dismiss(item.id)}>
            <XIcon />
          </Button>
        </Alert.Action>
      </Alert.Root>
    {/each}

    {#if openLogsFailed}
      <p class="text-sm text-destructive" role="alert">{m.desktop_startup_open_logs_failed()}</p>
    {/if}
  </section>
{/if}
