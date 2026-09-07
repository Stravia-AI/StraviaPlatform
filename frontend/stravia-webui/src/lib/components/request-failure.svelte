<script lang="ts">
import type { Snippet } from 'svelte'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'
import * as m from '$lib/paraglide/messages.js'
import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  title?: string
  message: string
  retry?: () => unknown
  retrying?: boolean
  class?: string
  children?: Snippet
}

let { title, message, retry, retrying = false, class: className, children }: Props = $props()
</script>

<Alert.Root variant="destructive" class={className}>
  <CircleAlertIcon />
  {#if title}<Alert.Title role="heading" aria-level={2}>{title}</Alert.Title>{/if}
  <Alert.Description>
    <p>{message}</p>
    {@render children?.()}
    {#if retry}
      <Button type="button" variant="outline" size="sm" disabled={retrying} onclick={() => retry?.()}>
        {#if retrying}<Spinner data-icon="inline-start" />{/if}
        {m.common_retry()}
      </Button>
    {/if}
  </Alert.Description>
</Alert.Root>
