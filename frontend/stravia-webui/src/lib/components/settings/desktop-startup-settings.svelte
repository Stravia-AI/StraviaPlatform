<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'

import { getDesktopClientSettings, setDesktopLaunchAtLogin, setDesktopSilentStart } from '#lib/desktop-client.js'
import type { DesktopClientSettings } from '#lib/desktop-client.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

const queryClient = useQueryClient()
const clientQuery = createQuery(() => ({ queryKey: ['desktop-client-settings'], queryFn: getDesktopClientSettings }))

let startupError = $state<string>()
let startupToggling = $state(false)

const clientState = $derived(clientQuery.data)

async function applyStartupToggle(
  apply: (enabled: boolean) => Promise<DesktopClientSettings>,
  enabled: boolean,
): Promise<void> {
  startupToggling = true
  startupError = undefined
  try {
    const next = await apply(enabled)
    queryClient.setQueryData(['desktop-client-settings'], next)
  } catch (error) {
    startupError = error instanceof Error ? error.message : String(error)
  } finally {
    startupToggling = false
  }
}
</script>

<SettingsSection id="startup" title={m.settings_startup()}>
  {#if clientState}
    <SettingRow title={m.desktop_client_launch_at_login()} labelFor="desktop-launch-at-login" layout="inline">
      {#if startupToggling}<Spinner aria-label={m.settings_saving()} />{/if}
      <Switch
        id="desktop-launch-at-login"
        bind:checked={
          () => clientState.launchAtLogin,
          (enabled: boolean) => void applyStartupToggle(setDesktopLaunchAtLogin, enabled)
        }
        disabled={startupToggling} />
    </SettingRow>
    {#if clientState.launchAtLogin}
      <SettingRow
        nested
        layout="inline"
        title={m.desktop_client_silent_start()}
        labelFor="desktop-silent-start"
        hint={m.desktop_client_silent_start_hint()}>
        <Switch
          id="desktop-silent-start"
          bind:checked={
            () => clientState.silentStart, (enabled: boolean) => void applyStartupToggle(setDesktopSilentStart, enabled)
          }
          disabled={startupToggling} />
      </SettingRow>
    {/if}
    {#if startupError}
      <Field.FieldError role="alert">
        {m.desktop_client_settings_not_saved()}
        <span class="mt-1 block font-normal text-muted-foreground">{startupError}</span>
      </Field.FieldError>
    {/if}
  {:else if clientQuery.isError}
    <div class="flex flex-wrap items-center gap-3 py-3 text-sm text-muted-foreground">
      <p>{m.desktop_client_settings_unavailable()}</p>
      <Button variant="outline" size="sm" onclick={() => void clientQuery.refetch()}>{m.common_retry()}</Button>
    </div>
  {:else}
    <p role="status" class="flex items-center gap-2 py-3 text-sm text-muted-foreground">
      <Spinner />{m.desktop_client_loading()}
    </p>
  {/if}
</SettingsSection>
