<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import DownloadIcon from '@lucide/svelte/icons/download'
import ExternalLinkIcon from '@lucide/svelte/icons/external-link'
import XIcon from '@lucide/svelte/icons/x'

import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import type { AvailableUpdate } from '$lib/product-update'

let {
  update,
  downloadSupported,
  onDismiss,
  onDownload,
  onViewRelease,
  onSkip,
}: {
  update: AvailableUpdate
  downloadSupported: boolean
  onDismiss: () => void
  onDownload: () => void
  onViewRelease: () => void
  onSkip: () => void
} = $props()
</script>

<Alert.Root role="status" class="w-full">
  <Alert.Title class="pr-8">{m.settings_update_notification_title()}</Alert.Title>
  <Button
    type="button"
    class="absolute top-2 right-2"
    size="icon-sm"
    variant="ghost"
    aria-label={m.common_close()}
    onclick={onDismiss}><XIcon /></Button>
  <Alert.Description>
    <p>{m.settings_update_notification_body({ version: update.version })}</p>
    <div class="flex flex-wrap gap-2">
      {#if downloadSupported && update.download_available}
        <Button type="button" onclick={onDownload}>
          <DownloadIcon data-icon="inline-start" />{m.settings_update_download()}
        </Button>
      {:else}
        <Button type="button" onclick={onViewRelease}>
          <ExternalLinkIcon data-icon="inline-start" />{m.settings_update_view_release()}
        </Button>
      {/if}
      <Button type="button" variant="ghost" onclick={onSkip}>
        {m.settings_update_skip()}
      </Button>
    </div>
  </Alert.Description>
</Alert.Root>
