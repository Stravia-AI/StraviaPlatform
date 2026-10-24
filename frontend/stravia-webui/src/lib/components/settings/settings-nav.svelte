<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import DatabaseIcon from '@lucide/svelte/icons/database'
import GlobeIcon from '@lucide/svelte/icons/globe'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import UserRoundIcon from '@lucide/svelte/icons/user-round'
import WrenchIcon from '@lucide/svelte/icons/wrench'

import { tabsListVariants, tabsTriggerVariants } from '#lib/components/ui/tabs/index.js'
import { settingsGroupHref, settingsGroupLabels, type SettingsGroup } from '#lib/settings-groups.js'

interface Props {
  groups: readonly SettingsGroup[]
  current: SettingsGroup
}

let { groups, current }: Props = $props()

const icons = {
  general: SlidersHorizontalIcon,
  network: GlobeIcon,
  data: DatabaseIcon,
  account: UserRoundIcon,
  advanced: WrenchIcon,
} as const
</script>

<!--
  同一组链接两种形态，随 route 容器宽度切换（不同时渲染两套）：
  窄于 64rem 为页头下方的横向页签（Tabs 分段样式，只显示文字，允许换行）；
  不小于 64rem 为设置列左侧 `--settings-nav-width` 宽的纵向导航（图标 + 文字，sticky）。
-->
<nav class="settings-nav {tabsListVariants()}" data-variant="default" aria-label={m.settings_sections()}>
  {#each groups as group (group)}
    {@const Icon = icons[group]}
    <a
      class="settings-nav-link {tabsTriggerVariants()}"
      data-state={current === group ? 'active' : 'inactive'}
      aria-current={current === group ? 'page' : undefined}
      href={resolve(settingsGroupHref(group))}>
      <Icon class="settings-nav-icon" aria-hidden="true" />
      <span class="min-w-0">{settingsGroupLabels[group]()}</span>
    </a>
  {/each}
</nav>

<style>
.settings-nav :global(.settings-nav-icon) {
  display: none;
}

@container route-page (min-width: 64rem) {
  .settings-nav {
    position: sticky;
    top: 0;
    width: var(--settings-nav-width);
    flex: none;
    flex-direction: column;
    flex-wrap: nowrap;
    align-items: stretch;
    gap: 0.125rem;
    border-radius: 0;
    background: transparent;
    padding: 0;
  }

  .settings-nav-link {
    width: 100%;
    justify-content: flex-start;
    gap: 0.5rem;
    border-radius: 6px;
    border-color: transparent;
    background: transparent;
    padding: 0.4375rem 0.75rem;
    font-size: 13px;
    font-weight: 500;
    line-height: 1.25rem;
    box-shadow: none;
    color: var(--muted-foreground);
  }

  .settings-nav-link :global(.settings-nav-icon) {
    display: block;
    width: 1rem;
    height: 1rem;
    flex: none;
  }

  .settings-nav-link:hover {
    background: color-mix(in oklab, var(--accent) 55%, transparent);
    color: var(--accent-foreground);
  }

  .settings-nav-link[data-state='active'] {
    border-color: transparent;
    background: var(--accent);
    color: var(--accent-foreground);
    box-shadow: none;
  }
}
</style>
