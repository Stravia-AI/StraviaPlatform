<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { page } from '$app/state'
import { createQuery } from '@tanstack/svelte-query'
import { tick } from 'svelte'

import { isTauri } from '#lib/admin-client.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import { DraftGuard } from '#lib/draft-guard.svelte.js'
import { provideInlineDraftRegistry } from '#lib/inline-draft-registry.svelte.js'
import { getAuthState } from '#lib/auth.js'
import AdminCredentialsSettings from '#lib/components/settings/admin-credentials-settings.svelte'
import AppearanceSettings from '#lib/components/settings/appearance-settings.svelte'
import ArtifactSettings from '#lib/components/settings/artifact-settings.svelte'
import DesktopServiceSettings from '#lib/components/settings/desktop-service-settings.svelte'
import DesktopStartupSettings from '#lib/components/settings/desktop-startup-settings.svelte'
import DiagnosticsSettings from '#lib/components/settings/diagnostics-settings.svelte'
import ProxySettings from '#lib/components/settings/proxy-settings.svelte'
import RequestHistorySettings from '#lib/components/settings/request-history-settings.svelte'
import SettingsNav from '#lib/components/settings/settings-nav.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import ProductUpdateSettings from '#lib/components/product-update-settings.svelte'
import RpmManagement from '#lib/components/rpm-management.svelte'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { availableSettingsGroups, resolveSettingsGroup } from '#lib/settings-groups.js'

// 行内输入的未保存草稿：切换分组或离开设置页前确认，没有草稿时不打扰。
const inlineDrafts = provideInlineDraftRegistry()
const guard = new DraftGuard({ dirty: () => inlineDrafts.dirty, busy: () => false })

const authStateQuery = createQuery(() => ({ queryKey: ['auth-state'], queryFn: getAuthState }))

const isServer = $derived(authStateQuery.data?.mode === 'server' || authStateQuery.data?.mode === 'unavailable')
const groups = $derived(availableSettingsGroups({ desktop: isTauri, server: isServer }))
const requested = $derived(page.url.searchParams.get('view'))
// 账号分组取决于异步的认证状态：加载期间保持请求的分组，避免先闪回「通用」。
const waitingForHost = $derived(requested === 'account' && authStateQuery.isPending)
const group = $derived(waitingForHost ? 'account' : resolveSettingsGroup(requested, groups))
const groupTitle = $derived(
  {
    general: m.settings_group_general,
    network: m.settings_group_network,
    data: m.settings_group_data,
    account: m.settings_group_account,
    advanced: m.settings_group_advanced,
  }[group](),
)

// 跨页入口附带分区锚点（如 `?view=network#client`）：分组渲染后滚动到该分区。
$effect(() => {
  const hash = page.url.hash.slice(1)
  void group
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

<div class="route-page">
  <PageHeader title={m.settings_settings()} />

  <div class="settings-layout">
    <SettingsNav {groups} current={group} />
    <div class="settings-column">
      {#if group === 'general'}
        <AppearanceSettings />
        {#if isTauri}<DesktopStartupSettings />{/if}
        <ProductUpdateSettings />
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

/* 导航形态随 route 容器宽度切换：不小于 64rem 时纵向导航在左，设置列不少于 50rem。 */
@container route-page (min-width: 64rem) {
  .settings-layout {
    flex-direction: row;
    align-items: flex-start;
    gap: 2rem;
  }

  .settings-column {
    flex: 1;
  }
}
</style>
