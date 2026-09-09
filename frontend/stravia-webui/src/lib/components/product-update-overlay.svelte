<script lang="ts">
import { browser } from '$app/environment'
import * as m from '$lib/paraglide/messages.js'
import { onDestroy, untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import UpdateNotification from '$lib/components/update-notification.svelte'
import * as Alert from '$lib/components/ui/alert'

import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import { Spinner } from '$lib/components/ui/spinner'
import { openExternalUrl } from '$lib/open-external'
import { supportsInAppInstallProgress } from '$lib/product-update'
import { getProductUpdateCoordinator } from '$lib/product-update.svelte'

const updates = getProductUpdateCoordinator()
const showInstallingOverlay = $derived(
  updates.state.phase === 'installing' && supportsInAppInstallProgress(browser ? navigator.userAgent : ''),
)

const notificationId = 'product-update-available'

// Sonner 只承接展示，关闭通知与跳过版本的规则仍由更新协调器持有。
$effect(() => {
  const update = updates.notification
  const downloadSupported = updates.status?.download_supported ?? false
  untrack(() => {
    if (!update) {
      toast.dismiss(notificationId)
      return
    }
    const dismiss = () => {
      if (updates.notification?.version === update.version) updates.dismissNotification()
    }
    toast.custom(UpdateNotification, {
      id: notificationId,
      style: 'pointer-events: auto;',
      duration: Number.POSITIVE_INFINITY,
      position: 'bottom-right',
      onDismiss: dismiss,
      componentProps: {
        update,
        downloadSupported,
        onDismiss: dismiss,
        onDownload: () => void updates.downloadAvailableUpdate(),
        onViewRelease: () => void openExternalUrl(update.release_url),
        onSkip: () => void updates.skipAvailableVersion(),
      },
    })
  })
})

onDestroy(() => toast.dismiss(notificationId))

function handleInstallPrompt(open: boolean): void {
  if (!open) updates.dismissInstallPrompt()
}
</script>

<Dialog.Root open={updates.state.installPromptOpen} onOpenChange={handleInstallPrompt}>
  <Dialog.Layout>
    {#snippet header()}
      <Dialog.Title>
        {m.settings_update_install_title({ version: updates.state.targetVersion ?? '' })}
      </Dialog.Title>
      <Dialog.Description>{m.settings_update_install_warning()}</Dialog.Description>
    {/snippet}
    {#if updates.state.downloadedReleaseUrl}
      <button
        type="button"
        class="w-fit text-sm font-medium text-primary underline-offset-4 hover:underline"
        onclick={() => void openExternalUrl(updates.state.downloadedReleaseUrl!)}>
        {m.settings_update_view_release()}
      </button>
    {/if}
    {#if updates.state.error}
      <Alert.Root variant="destructive">
        <Alert.Description>{m.settings_update_install_failed({ message: updates.state.error })}</Alert.Description>
      </Alert.Root>
    {/if}
    {#snippet footer()}
      <Button type="button" onclick={() => void updates.installDownloadedUpdate()}
        >{m.settings_update_install()}</Button>
    {/snippet}
  </Dialog.Layout>
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
