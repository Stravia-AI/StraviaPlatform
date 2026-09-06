<script lang="ts">
import { browser } from '$app/environment'
import * as m from '$lib/paraglide/messages.js'
import DownloadIcon from '@lucide/svelte/icons/download'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import XIcon from '@lucide/svelte/icons/x'

import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import { Spinner } from '$lib/components/ui/spinner'
import { openExternalUrl } from '$lib/open-external'
import { supportsInAppInstallProgress } from '$lib/product-update'
import { getProductUpdateCoordinator } from '$lib/product-update.svelte'

const updates = getProductUpdateCoordinator()
const showInstallingOverlay = $derived(
  updates.state.phase === 'installing' &&
    supportsInAppInstallProgress(browser ? navigator.userAgent : ''),
)

function handleInstallPrompt(open: boolean): void {
  if (!open) updates.dismissInstallPrompt()
}
</script>

{#if updates.notification}
  <aside
    class="fixed right-4 bottom-4 z-40 grid w-[min(24rem,calc(100vw-2rem))] gap-3 rounded-xl border bg-popover p-4 text-popover-foreground shadow-xl"
    aria-live="polite">
    <Button
      class="absolute top-2 right-2"
      size="icon-sm"
      variant="ghost"
      aria-label={m.common_close()}
      onclick={() => updates.dismissNotification()}><XIcon /></Button>
    <div class="pr-8">
      <p class="font-semibold">{m.settings_update_notification_title()}</p>
      <p class="mt-1 text-sm text-muted-foreground">
        {m.settings_update_notification_body({ version: updates.notification.version })}
      </p>
    </div>
    <div class="flex flex-wrap gap-2">
      {#if updates.status?.download_supported && updates.notification.download_available}
        <Button onclick={() => void updates.downloadAvailableUpdate()}>
          <DownloadIcon data-icon="inline-start" />{m.settings_update_download()}
        </Button>
      {:else}
        <Button onclick={() => void openExternalUrl(updates.notification!.release_url)}>
          <ExternalLinkIcon data-icon="inline-start" />{m.settings_update_view_release()}
        </Button>
      {/if}
      <Button variant="ghost" onclick={() => void updates.skipAvailableVersion()}>
        {m.settings_update_skip()}
      </Button>
    </div>
  </aside>
{/if}

<Dialog.Root open={updates.state.installPromptOpen} onOpenChange={handleInstallPrompt}>
  <Dialog.Content>
    <Dialog.Header>
      <Dialog.Title>
        {m.settings_update_install_title({ version: updates.state.targetVersion ?? '' })}
      </Dialog.Title>
      <Dialog.Description>{m.settings_update_install_warning()}</Dialog.Description>
    </Dialog.Header>
    {#if updates.state.downloadedReleaseUrl}
      <button
        type="button"
        class="w-fit text-sm font-medium text-primary underline-offset-4 hover:underline"
        onclick={() => void openExternalUrl(updates.state.downloadedReleaseUrl!)}>
        {m.settings_update_view_release()}
      </button>
    {/if}
    {#if updates.state.error}
      <p class="text-sm text-destructive" role="alert">
        {m.settings_update_install_failed({ message: updates.state.error })}
      </p>
    {/if}
    <Dialog.Footer>
      <Button onclick={() => void updates.installDownloadedUpdate()}>{m.settings_update_install()}</Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>

{#if showInstallingOverlay}
  <div class="fixed inset-0 z-50 grid place-items-center bg-background/85 backdrop-blur-sm" aria-live="assertive">
    <div class="grid justify-items-center gap-3 rounded-xl border bg-popover p-6 shadow-xl">
      <Spinner class="size-6" />
      <p class="font-medium">
        {m.settings_update_installing({ version: updates.state.targetVersion ?? '' })}
      </p>
    </div>
  </div>
{/if}
