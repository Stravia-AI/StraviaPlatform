<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import PencilIcon from '@lucide/svelte/icons/pencil'
import { createQuery } from '@tanstack/svelte-query'

import { loadRpm, rpmQueryKey } from '#lib/rpm.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import RpmForm from './rpm-form.svelte'

// 移动端行没有表头，showLabel 让摘要自带「RPM」前缀。
let {
  providerId,
  modelId,
  modelName,
  showLabel = false,
  compact = false,
}: { providerId: string; modelId: string; modelName: string; showLabel?: boolean; compact?: boolean } = $props()

const query = createQuery(() => ({ queryKey: rpmQueryKey, queryFn: loadRpm }))
const destination = $derived(
  query.data?.destinations.find((item) => item.provider_id === providerId && item.model === modelId),
)
const summary = $derived(
  query.isPending
    ? m.common_settings_loading()
    : query.error || !query.data
      ? m.common_unavailable()
      : destination?.rpm_limit
        ? m.rpm_value({ limit: destination.rpm_limit })
        : m.api_key_editor_unlimited(),
)
let open = $state(false)
</script>

<Popover.Root bind:open>
  <Popover.Trigger>
    {#snippet child({ props })}
      <Button
        {...props}
        variant="ghost"
        class={[
          'max-w-full justify-start px-2 text-left font-normal',
          compact && 'h-auto min-h-7 gap-1.5 px-1.5 py-0.5',
        ]}
        aria-label={m.rpm_edit_label({ model: modelId, value: summary })}
        disabled={query.isPending}>
        {#if query.isPending}<Spinner />{:else if query.error}{m.common_unavailable()}{:else}<span class="truncate"
            >{#if showLabel}<span class="text-muted-foreground">RPM · </span>{/if}{summary}</span
          >{/if}
        {#if compact && !query.isPending && !query.error}<PencilIcon
            class="size-3 shrink-0 text-muted-foreground"
            aria-hidden="true" />{/if}
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="flex w-80 max-w-[calc(100vw-2rem)] flex-col p-0" role="dialog">
    <Popover.Header class="border-b border-border/60 px-4 py-3">
      <Popover.Title class="truncate">{modelName || modelId}</Popover.Title>
      {#if modelName && modelName !== modelId}
        <Popover.Description class="truncate font-technical text-xs">{modelId}</Popover.Description>
      {/if}
    </Popover.Header>
    {#if open}
      <RpmForm {providerId} {modelId} onSaved={() => (open = false)} onCancel={() => (open = false)} />
    {/if}
  </Popover.Content>
</Popover.Root>
