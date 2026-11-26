<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down'
import { createQuery } from '@tanstack/svelte-query'

import { admin, PROXY_MODE_NONE, PROXY_MODE_SYSTEM } from '#lib/admin-client.js'
import { localeState } from '#lib/localization.svelte.js'
import { proxyDisplayName, proxyHostPort, redactProxyUrl } from '#lib/proxy-display.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Command from '#lib/components/ui/command/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import { cn } from '#lib/utils.js'

interface Props {
  value: string
  onchange: (value: string) => void
  disabled?: boolean
  id?: string
  ariaLabel?: string
  ariaLabelledby?: string
  ariaDescribedby?: string
  class?: string
}

let {
  value,
  onchange,
  disabled = false,
  id,
  ariaLabel,
  ariaLabelledby,
  ariaDescribedby,
  class: className,
}: Props = $props()

let open = $state(false)
let query = $state('')

const proxyQuery = createQuery(() => ({
  queryKey: ['outbound-proxy-settings'],
  queryFn: admin.settings.proxy,
}))

const systemProxyQuery = createQuery(() => ({
  queryKey: ['outbound-proxy-system'],
  queryFn: admin.settings.systemProxy,
}))

const proxies = $derived(proxyQuery.data?.proxies ?? [])
const systemDetectedUrl = $derived(
  systemProxyQuery.data?.detected_url ? redactProxyUrl(systemProxyQuery.data.detected_url) : null,
)
const systemDetail = $derived.by(() => {
  if (!systemProxyQuery.data) return undefined
  return systemDetectedUrl ?? m.proxy_system_direct()
})

const selectedSavedProxy = $derived(
  value !== PROXY_MODE_NONE && value !== PROXY_MODE_SYSTEM
    ? proxies.find((entry) => entry.id === value)
    : undefined,
)
const isMissingProxy = $derived(
  Boolean(value) &&
    value !== PROXY_MODE_NONE &&
    value !== PROXY_MODE_SYSTEM &&
    Boolean(proxyQuery.data) &&
    !selectedSavedProxy,
)

const triggerDisplay = $derived.by((): { label: string; technical: boolean; muted: boolean; warning: boolean } => {
  if (value === PROXY_MODE_NONE) {
    return { label: m.proxy_mode_none(), technical: false, muted: false, warning: false }
  }
  if (value === PROXY_MODE_SYSTEM) {
    return { label: m.proxy_mode_system(), technical: false, muted: false, warning: false }
  }
  if (selectedSavedProxy) {
    const named = selectedSavedProxy.name.trim().length > 0
    return { label: proxyDisplayName(selectedSavedProxy), technical: !named, muted: false, warning: false }
  }
  if (proxyQuery.isPending) {
    return { label: m.common_loading(), technical: false, muted: true, warning: false }
  }
  if (isMissingProxy) {
    return { label: m.proxy_select_missing({ id: value }), technical: false, muted: false, warning: true }
  }
  return { label: value || m.proxy_mode_system(), technical: Boolean(value), muted: !value, warning: false }
})

const needle = $derived(query.trim().toLocaleLowerCase(localeState.current))

const fixedOptions = $derived.by(() => {
  const items = [
    {
      value: PROXY_MODE_NONE,
      label: m.proxy_mode_none(),
      detail: m.proxy_mode_none_detail(),
      technicalDetail: false,
      searchText: `${m.proxy_mode_none()} ${m.proxy_mode_none_detail()} none direct`,
    },
    {
      value: PROXY_MODE_SYSTEM,
      label: m.proxy_mode_system(),
      detail: systemDetail,
      technicalDetail: Boolean(systemDetectedUrl),
      searchText: `${m.proxy_mode_system()} ${systemDetail ?? ''} system os env`,
    },
  ]
  if (!needle) return items
  return items.filter((item) => item.searchText.toLocaleLowerCase(localeState.current).includes(needle))
})

const savedOptions = $derived.by(() => {
  const mapped = proxies.map((entry) => {
    const named = entry.name.trim().length > 0
    const label = proxyDisplayName(entry)
    const redacted = redactProxyUrl(entry.url)
    const hostPort = proxyHostPort(entry.url)
    return {
      id: entry.id,
      label,
      named,
      redacted,
      searchText: `${entry.name} ${hostPort} ${redacted}`,
    }
  })
  if (!needle) return mapped
  return mapped.filter((item) => item.searchText.toLocaleLowerCase(localeState.current).includes(needle))
})

const hasAnyMatch = $derived(fixedOptions.length > 0 || savedOptions.length > 0)

function choose(next: string): void {
  open = false
  query = ''
  if (next !== value) onchange(next)
}
</script>

<Popover.Root
  bind:open
  onOpenChange={(next: boolean) => {
    if (!next) query = ''
  }}>
  <Popover.Trigger>
    {#snippet child({ props }: { props: Record<string, unknown> })}
      <Button
        {...props}
        id={id ?? (typeof props.id === 'string' ? props.id : undefined)}
        type="button"
        variant="outline"
        class={cn('w-full min-w-0 justify-between font-normal', className)}
        role="combobox"
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-label={ariaLabel}
        aria-labelledby={ariaLabelledby}
        aria-describedby={ariaDescribedby}
        {disabled}>
        <span
          class={cn(
            'truncate text-left',
            triggerDisplay.technical && 'font-technical',
            triggerDisplay.muted && 'text-muted-foreground',
            triggerDisplay.warning && 'text-warning',
          )}>
          {triggerDisplay.label}
        </span>
        <ChevronsUpDownIcon data-icon="inline-end" class="ml-auto shrink-0 text-muted-foreground" />
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="w-(--bits-popover-anchor-width) min-w-64 max-w-[calc(100vw-2rem)] p-0">
    <Command.Root shouldFilter={false} label={m.proxy_select_search_placeholder()}>
      <Command.Input
        bind:value={query}
        placeholder={m.proxy_select_search_placeholder()}
        aria-label={m.proxy_select_search_placeholder()} />
      <Command.List class="max-h-[min(18rem,var(--bits-popover-content-available-height))]">
        {#if !hasAnyMatch}
          <Command.Empty>{m.proxy_select_empty()}</Command.Empty>
        {:else}
          {#if fixedOptions.length > 0}
            <Command.Group>
              {#each fixedOptions as option (option.value)}
                <Command.Item
                  value={option.value}
                  data-checked={value === option.value}
                  aria-selected={value === option.value}
                  onSelect={() => choose(option.value)}>
                  <span class="flex min-w-0 flex-1 flex-col gap-0.5">
                    <span class="truncate">{option.label}</span>
                    {#if option.detail}
                      <span
                        class={cn(
                          'truncate text-xs text-muted-foreground',
                          option.technicalDetail && 'font-technical',
                        )}>
                        {option.detail}
                      </span>
                    {/if}
                  </span>
                </Command.Item>
              {/each}
            </Command.Group>
          {/if}
          {#if fixedOptions.length > 0 && savedOptions.length > 0}
            <Command.Separator />
          {/if}
          {#if savedOptions.length > 0}
            <Command.Group heading={m.proxy_select_saved_group()}>
              {#each savedOptions as option (option.id)}
                <Command.Item
                  value={option.id}
                  data-checked={value === option.id}
                  aria-selected={value === option.id}
                  onSelect={() => choose(option.id)}>
                  <span class="flex min-w-0 flex-1 flex-col gap-0.5">
                    <span class={cn('truncate', !option.named && 'font-technical')}>{option.label}</span>
                    {#if option.redacted}
                      <span class="truncate font-technical text-xs text-muted-foreground">{option.redacted}</span>
                    {/if}
                  </span>
                </Command.Item>
              {/each}
            </Command.Group>
          {/if}
        {/if}
      </Command.List>
    </Command.Root>
  </Popover.Content>
</Popover.Root>
