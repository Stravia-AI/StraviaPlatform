<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import ArrowUpRightIcon from '@lucide/svelte/icons/arrow-up-right'
import GlobeIcon from '@lucide/svelte/icons/globe'
import StarIcon from '@lucide/svelte/icons/star'

import { capabilitiesQueryOptions } from '#lib/admin-client.js'
import { openExternalUrl } from '#lib/open-external.js'
import BrandMark from '#lib/components/brand-mark.svelte'
import BrandWordmark from '#lib/components/brand-wordmark.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'

const websiteUrl = 'https://stravia.cn'
const websiteHost = 'stravia.cn'
const githubUrl = 'https://github.com/Stravia-AI/StraviaPlatform'
const githubRepo = 'Stravia-AI/StraviaPlatform'

const capabilitiesQuery = createQuery(() => capabilitiesQueryOptions())

const capabilities = $derived(
  capabilitiesQuery.isPending || capabilitiesQuery.isError ? undefined : capabilitiesQuery.data,
)

const isCommercial = $derived(capabilities?.edition === 'commercial')

const editionLabel = $derived.by(() => {
  if (!capabilities) return null
  if (capabilities.edition === 'ce') return m.settings_about_edition_ce()
  if (capabilities.license_tier === 'pro') return m.settings_about_edition_pro()
  if (capabilities.license_tier === 'enterprise') return m.settings_about_edition_enterprise()
  return m.settings_about_edition_commercial()
})

const licenseNotice = $derived.by((): { label: string; tone: 'warning' | 'error' } | null => {
  if (!capabilities || capabilities.edition !== 'commercial') return null
  switch (capabilities.license_status) {
    case 'expired':
      return { label: m.settings_about_license_expired(), tone: 'warning' }
    case 'invalid':
      return { label: m.settings_about_license_invalid(), tone: 'error' }
    case 'missing':
      return { label: m.settings_about_license_missing(), tone: 'warning' }
    default:
      return null
  }
})
</script>

<SettingsSection id="about" title={m.settings_about()}>
  <div class="@container/about mt-2 divide-y divide-border overflow-hidden rounded-xl border border-border bg-card">
    <!-- 品牌铭牌头 -->
    <div
      class="flex flex-col gap-3 bg-muted/25 px-4 py-4 @min-[30rem]/about:flex-row @min-[30rem]/about:items-center @min-[30rem]/about:justify-between @min-[30rem]/about:gap-4 sm:px-5 dark:bg-muted/20">
      <div class="flex min-w-0 items-center gap-3">
        <div
          class="flex size-9 shrink-0 items-center justify-center rounded-lg border border-border bg-card shadow-2xs">
          <BrandMark class="size-5.5" />
        </div>
        <div class="min-w-0">
          <div class="flex items-center">
            <BrandWordmark class="text-lg text-foreground" />
          </div>
          <p class="mt-0.5 text-xs text-pretty text-muted-foreground">{m.app_shell_local_ai_gateway()}</p>
        </div>
      </div>

      <div class="flex flex-wrap items-center gap-2 @min-[30rem]/about:justify-end">
        <span class="text-xs text-muted-foreground">{m.settings_about_edition()}</span>
        {#if editionLabel}
          {#if isCommercial}
            <Badge
              variant="outline"
              class="border-primary/25 bg-primary/10 text-primary dark:border-primary/35 dark:bg-primary/15">
              {editionLabel}
            </Badge>
          {:else}
            <Badge variant="secondary" class="border-border/75">{editionLabel}</Badge>
          {/if}
          {#if licenseNotice}
            <StatusIndicator compact label={licenseNotice.label} tone={licenseNotice.tone} />
          {/if}
        {:else}
          <span class="font-technical text-sm text-muted-foreground">–</span>
        {/if}
      </div>
    </div>

    <!-- 官方网站入口 -->
    <button
      type="button"
      title={websiteUrl}
      onclick={() => void openExternalUrl(websiteUrl)}
      class="group/link flex w-full min-w-0 items-center justify-between gap-3 px-4 py-3.5 text-left transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:bg-accent/50 focus-visible:bg-accent/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none focus-visible:ring-inset sm:px-5">
      <div class="flex min-w-0 items-center gap-3">
        <span
          class="flex size-9 shrink-0 items-center justify-center rounded-lg border border-border/80 bg-background text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/link:border-primary/35 group-hover/link:text-primary"
          aria-hidden="true">
          <GlobeIcon class="size-5" />
        </span>
        <div class="min-w-0">
          <span class="block text-sm font-medium text-foreground">{m.settings_about_website()}</span>
          <span class="font-technical block truncate text-xs text-muted-foreground">{websiteHost}</span>
        </div>
      </div>
      <span
        class="flex shrink-0 items-center gap-1 text-xs font-medium text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/link:text-foreground">
        <span>{m.settings_about_open_website()}</span>
        <ArrowUpRightIcon
          class="size-3.5 transition-transform duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/link:translate-x-0.5 group-hover/link:-translate-y-0.5"
          aria-hidden="true" />
      </span>
    </button>

    <!-- GitHub 仓库与 Star 引导 -->
    <div
      class="flex flex-col gap-3 px-4 py-4 @min-[30rem]/about:flex-row @min-[30rem]/about:items-center @min-[30rem]/about:justify-between @min-[30rem]/about:gap-4 sm:px-5">
      <div class="flex min-w-0 items-start gap-3">
        <span
          class="mt-0.5 flex size-9 shrink-0 items-center justify-center rounded-lg border border-border/80 bg-background text-foreground/85"
          aria-hidden="true">
          <svg class="size-4.5" viewBox="0 0 24 24" fill="currentColor" fill-rule="evenodd" aria-hidden="true">
            <path
              d="M12 0c6.63 0 12 5.276 12 11.79-.001 5.067-3.29 9.567-8.175 11.187-.6.118-.825-.25-.825-.56 0-.398.015-1.665.015-3.242 0-1.105-.375-1.813-.81-2.181 2.67-.295 5.475-1.297 5.475-5.822 0-1.297-.465-2.344-1.23-3.169.12-.295.54-1.503-.12-3.125 0 0-1.005-.324-3.3 1.209a11.32 11.32 0 00-3-.398c-1.02 0-2.04.133-3 .398-2.295-1.518-3.3-1.209-3.3-1.209-.66 1.622-.24 2.83-.12 3.125-.765.825-1.23 1.887-1.23 3.169 0 4.51 2.79 5.527 5.46 5.822-.345.294-.66.81-.765 1.577-.69.31-2.415.81-3.495-.973-.225-.354-.9-1.223-1.845-1.209-1.005.015-.405.56.015.781.51.28 1.095 1.327 1.23 1.666.24.663 1.02 1.93 4.035 1.385 0 .988.015 1.916.015 2.196 0 .31-.225.664-.825.56C3.303 21.374-.003 16.867 0 11.791 0 5.276 5.37 0 12 0z" />
          </svg>
        </span>
        <div class="min-w-0 flex-1">
          <div class="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
            <span class="text-sm font-medium text-foreground">{m.settings_about_github()}</span>
            <button
              type="button"
              title={githubUrl}
              onclick={() => void openExternalUrl(githubUrl)}
              class="font-technical inline-flex max-w-full min-w-0 items-center rounded-xs text-xs text-muted-foreground underline-offset-4 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:text-primary hover:underline focus-visible:text-primary">
              <span class="truncate">{githubRepo}</span>
            </button>
          </div>
          <p class="mt-1 text-xs leading-relaxed text-pretty text-muted-foreground">
            {m.settings_about_github_description()}
          </p>
        </div>
      </div>

      <div class="shrink-0 @max-[30rem]/about:ps-12">
        <Button
          type="button"
          variant="outline"
          class="group/star border-warning/35 bg-warning/[0.06] text-foreground hover:border-warning/55 hover:bg-warning/[0.12] dark:border-warning/35 dark:bg-warning/[0.09] dark:hover:border-warning/55 dark:hover:bg-warning/[0.16]"
          onclick={() => void openExternalUrl(githubUrl)}>
          <StarIcon
            data-icon="inline-start"
            class="fill-warning/25 text-warning transition-[fill,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/star:scale-110 group-hover/star:fill-warning" />
          {m.settings_about_github_star()}
        </Button>
      </div>
    </div>
  </div>
</SettingsSection>
