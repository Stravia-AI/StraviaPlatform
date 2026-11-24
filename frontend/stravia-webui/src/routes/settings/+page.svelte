<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { page } from '$app/state'
import { resolve } from '$app/paths'
import { createQuery } from '@tanstack/svelte-query'
import { tick } from 'svelte'

import { capabilitiesQueryOptions, isTauri } from '#lib/admin-client.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import { DraftGuard } from '#lib/draft-guard.svelte.js'
import { provideInlineDraftRegistry } from '#lib/inline-draft-registry.svelte.js'
import { getAuthState } from '#lib/auth.js'
import AdminCredentialsSettings from '#lib/components/settings/admin-credentials-settings.svelte'
import AppearanceSettings from '#lib/components/settings/appearance-settings.svelte'
import ArtifactSettings from '#lib/components/settings/artifact-settings.svelte'
import CurrencySettings from '#lib/components/settings/currency-settings.svelte'
import DesktopServiceSettings from '#lib/components/settings/desktop-service-settings.svelte'
import DesktopStartupSettings from '#lib/components/settings/desktop-startup-settings.svelte'
import DiagnosticsSettings from '#lib/components/settings/diagnostics-settings.svelte'
import PlatformLlmSettings from '#lib/components/settings/platform-llm-settings.svelte'
import ProxySettings from '#lib/components/settings/proxy-settings.svelte'
import RequestHistorySettings from '#lib/components/settings/request-history-settings.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import { ensureBuildExtensionsRegistered } from '#lib/extensions/index.js'
import SettingsNav from '#lib/components/settings/settings-nav.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import ProductUpdateSettings from '#lib/components/product-update-settings.svelte'
import RpmManagement from '#lib/components/rpm-management.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { availableSettingsGroups, resolveSettingsGroup, settingsGroupLabels } from '#lib/settings-groups.js'

// 行内输入的未保存草稿：切换分组或离开设置页前确认，没有草稿时不打扰。
const inlineDrafts = provideInlineDraftRegistry()
const guard = new DraftGuard({ dirty: () => inlineDrafts.dirty, busy: () => false })

const authStateQuery = createQuery(() => ({ queryKey: ['auth-state'], queryFn: getAuthState }))

const isServer = $derived(authStateQuery.data?.mode === 'server' || authStateQuery.data?.mode === 'unavailable')
const groups = $derived(availableSettingsGroups({ server: isServer }))
const requested = $derived(page.url.searchParams.get('view'))
// 账号分组取决于异步的认证状态：加载期间保持请求的分组，避免先闪回「通用」。
const waitingForHost = $derived(requested === 'account' && authStateQuery.isPending)
const group = $derived(waitingForHost ? 'account' : resolveSettingsGroup(requested, groups))

// 与壳层共用能力查询缓存：社区版构建不注册扩展模块，仅在扩展贡献非空时渲染插槽与分组。
const extensions = ensureBuildExtensionsRegistered()
const capabilitiesQuery = createQuery(() => capabilitiesQueryOptions())
const capabilities = $derived(capabilitiesQuery.data?.capabilities ?? [])

interface RenderableSlotContribution {
  id: string
  title: string | null
  description: string | null
  href?: string
}

interface RenderableExtensionSettingsGroup {
  id: string
  label: string
  description: string | null
  slots: RenderableSlotContribution[]
}

function renderableSlots(slot: string): RenderableSlotContribution[] {
  return extensions
    .activeSlots(capabilities, slot)
    .map((contribution) => ({
      id: contribution.id,
      title: contribution.title?.().trim() || null,
      description: contribution.description?.().trim() || null,
      href: contribution.href,
    }))
    .filter((contribution) => contribution.title !== null || contribution.description !== null)
}

const generalTopSlots = $derived(renderableSlots('settings:general:top'))
const extensionSettingsGroups = $derived.by((): RenderableExtensionSettingsGroup[] =>
  extensions
    .activeSettingsGroups(capabilities)
    .map((item) => ({
      id: item.id,
      label: item.label().trim(),
      description: item.description?.().trim() || null,
      slots: renderableSlots(`settings:${item.id}`),
    }))
    .filter((item) => item.label.length > 0),
)
const activeExtensionGroup = $derived(
  requested ? (extensionSettingsGroups.find((item) => item.id === requested) ?? null) : null,
)
const groupTitle = $derived(activeExtensionGroup ? activeExtensionGroup.label : settingsGroupLabels[group]())

// 跨页入口附带分区锚点（如 `?view=network#client`）：分组渲染后滚动到该分区。
$effect(() => {
  const hash = page.url.hash.slice(1)
  void group
  void activeExtensionGroup
  if (!hash) return
  void tick().then(() => {
    const section = document.getElementById(hash)
    const container = section?.closest<HTMLElement>('.shell-scrollbar')
    if (!section || !container) return
    container.scrollTop += section.getBoundingClientRect().top - container.getBoundingClientRect().top - 16
  })
})
</script>

<svelte:head><title>{groupTitle} · {m.settings_settings()} · Stravia</title></svelte:head>

{#snippet extensionSlotList(slotName: string, items: readonly RenderableSlotContribution[])}
  {#if items.length > 0}
    <div class="flex min-w-0 flex-col gap-2" data-extension-slot={slotName}>
      {#each items as contribution (contribution.id)}
        <div
          class="flex min-w-0 flex-col gap-0.5 rounded-lg border bg-card px-3 py-2.5 text-sm"
          data-extension-contribution={contribution.id}>
          {#if contribution.title && contribution.href}
            <a
              class="w-fit font-medium break-words text-pretty text-primary underline-offset-4 hover:underline"
              href={resolve(contribution.href)}>{contribution.title}</a>
          {:else if contribution.title}
            <p class="font-medium break-words text-pretty">{contribution.title}</p>
          {/if}
          {#if contribution.description}
            {#if !contribution.title && contribution.href}
              <a
                class="w-fit text-xs break-words text-pretty text-primary underline-offset-4 hover:underline"
                href={resolve(contribution.href)}>{contribution.description}</a>
            {:else}
              <p class="text-xs break-words text-pretty text-muted-foreground">{contribution.description}</p>
            {/if}
          {/if}
        </div>
      {/each}
    </div>
  {/if}
{/snippet}

{#snippet extensionSettingsGroupSection(extGroup: RenderableExtensionSettingsGroup)}
  <SettingsSection id={extGroup.id} title={extGroup.label}>
    {#if extGroup.description || extGroup.slots.length > 0}
      <div class="flex min-w-0 flex-col gap-2" data-extension-settings-group={extGroup.id}>
        {#if extGroup.description}
          <p class="text-sm break-words text-pretty text-muted-foreground">{extGroup.description}</p>
        {/if}
        {@render extensionSlotList(`settings:${extGroup.id}`, extGroup.slots)}
      </div>
    {/if}
  </SettingsSection>
{/snippet}

<div class="route-page route-page-column settings-page">
  <PageHeader title={m.settings_settings()} />

  <div class="settings-layout">
    <SettingsNav {groups} current={group} />
    <div class="settings-column">
      {#if activeExtensionGroup}
        {@render extensionSettingsGroupSection(activeExtensionGroup)}
      {:else if group === 'general'}
        {@render extensionSlotList('settings:general:top', generalTopSlots)}
        <AppearanceSettings />
        <CurrencySettings />
        <PlatformLlmSettings />
        {#if isTauri}<DesktopStartupSettings />{/if}
        <ProductUpdateSettings />
        {#if extensionSettingsGroups.length > 0}
          {#each extensionSettingsGroups as extGroup (extGroup.id)}
            {@render extensionSettingsGroupSection(extGroup)}
          {/each}
        {/if}
      {:else if group === 'network'}
        <ProxySettings />
        {#if isTauri}<DesktopServiceSettings />{/if}
      {:else if group === 'data'}
        <RequestHistorySettings />
        <ArtifactSettings />
      {:else if group === 'account'}
        {#if isServer}
          <AdminCredentialsSettings />
        {:else}
          <Skeleton class="h-9" aria-busy="true" />
        {/if}
      {:else}
        <RpmManagement />
        <DiagnosticsSettings />
      {/if}
    </div>
  </div>
</div>

<DraftDiscardDialog {guard} />

<style>
/* 窄时页签在页头下方，设置列居中；宽时导航与设置列作为一组居中，页头与导航左缘对齐。 */
.settings-page {
  --settings-nav-width: 12rem;
  --settings-layout-gap: 2rem;
}

.settings-layout {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 1.5rem;
}

.settings-column {
  display: flex;
  width: 100%;
  min-width: 0;
  max-width: var(--content-column-max-width);
  flex-direction: column;
  gap: 2rem;
}

/* 导航形态随 route 容器宽度切换：不小于 64rem 时纵向导航在左，设置列保持设置列宽度。 */
@container route-page (min-width: 64rem) {
  .settings-page > :global(*) {
    max-width: calc(var(--settings-nav-width) + var(--settings-layout-gap) + var(--content-column-max-width));
  }

  .settings-layout {
    flex-direction: row;
    align-items: flex-start;
    gap: var(--settings-layout-gap);
  }

  .settings-column {
    flex: 1;
  }
}
</style>
