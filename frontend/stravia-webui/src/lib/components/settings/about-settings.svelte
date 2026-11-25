<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import ArrowUpRightIcon from '@lucide/svelte/icons/arrow-up-right'
import BookOpenIcon from '@lucide/svelte/icons/book-open'
import CheckIcon from '@lucide/svelte/icons/check'
import CopyIcon from '@lucide/svelte/icons/copy'
import GlobeIcon from '@lucide/svelte/icons/globe'
import MessagesSquareIcon from '@lucide/svelte/icons/messages-square'
import QrCodeIcon from '@lucide/svelte/icons/qr-code'
import StarIcon from '@lucide/svelte/icons/star'
import { onDestroy } from 'svelte'
import type { MouseEventHandler } from 'svelte/elements'
import { toast } from 'svelte-sonner'

import { capabilitiesQueryOptions } from '#lib/admin-client.js'
import { copyText } from '#lib/clipboard.js'
import { openExternalUrl } from '#lib/open-external.js'
import BrandMark from '#lib/components/brand-mark.svelte'
import BrandWordmark from '#lib/components/brand-wordmark.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import wechatOfficialQr from '../../../assets/media/wechat-official-qr.jpg'

type BrandIconName = 'github' | 'wechat' | 'bilibili'
type CopyTarget = 'qq' | 'wechat'

const websiteUrl = 'https://www.stravia.cn/'
const docsUrl = 'https://docs.stravia.cn/'
const githubUrl = 'https://github.com/Stravia-AI/StraviaPlatform'
const bilibiliUrl = 'https://space.bilibili.com/3747568584494602'
const qqGroupNumber = '1125500160'
const wechatAccountName = 'Stravia'

const wechatTitleId = $props.id()

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

let copiedTarget = $state<CopyTarget | null>(null)
let copiedTimer: ReturnType<typeof setTimeout> | undefined
let disposed = false

onDestroy(() => {
  disposed = true
  clearTimeout(copiedTimer)
})

async function copyChannelValue(target: CopyTarget, value: string, trigger: HTMLElement): Promise<void> {
  try {
    await copyText(value, trigger)
    if (disposed) return
    copiedTarget = target
    clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => {
      copiedTarget = null
    }, 1600)
    toast.success(target === 'qq' ? m.settings_about_qq_copied() : m.settings_about_wechat_copied())
  } catch {
    if (disposed) return
    toast.error(m.common_not_copy_clipboard())
  }
}

const handleCopyQqGroup: MouseEventHandler<HTMLElement> = ({ currentTarget }) =>
  void copyChannelValue('qq', qqGroupNumber, currentTarget)

const handleCopyWechatName: MouseEventHandler<HTMLElement> = ({ currentTarget }) =>
  void copyChannelValue('wechat', wechatAccountName, currentTarget)
</script>

{#snippet brandIcon(name: BrandIconName)}
  {#if name === 'github'}
    <svg
      data-icon="inline-start"
      class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
      viewBox="0 0 24 24"
      fill="currentColor"
      fill-rule="evenodd"
      aria-hidden="true"
      focusable="false">
      <path
        d="M12 0c6.63 0 12 5.276 12 11.79-.001 5.067-3.29 9.567-8.175 11.187-.6.118-.825-.25-.825-.56 0-.398.015-1.665.015-3.242 0-1.105-.375-1.813-.81-2.181 2.67-.295 5.475-1.297 5.475-5.822 0-1.297-.465-2.344-1.23-3.169.12-.295.54-1.503-.12-3.125 0 0-1.005-.324-3.3 1.209a11.32 11.32 0 00-3-.398c-1.02 0-2.04.133-3 .398-2.295-1.518-3.3-1.209-3.3-1.209-.66 1.622-.24 2.83-.12 3.125-.765.825-1.23 1.887-1.23 3.169 0 4.51 2.79 5.527 5.46 5.822-.345.294-.66.81-.765 1.577-.69.31-2.415.81-3.495-.973-.225-.354-.9-1.223-1.845-1.209-1.005.015-.405.56.015.781.51.28 1.095 1.327 1.23 1.666.24.663 1.02 1.93 4.035 1.385 0 .988.015 1.916.015 2.196 0 .31-.225.664-.825.56C3.303 21.374-.003 16.867 0 11.791 0 5.276 5.37 0 12 0z" />
    </svg>
  {:else if name === 'wechat'}
    <!-- Simple Icons (CC0) monochrome path so the mark follows theme currentColor. -->
    <svg
      data-icon="inline-start"
      class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground group-aria-expanded/button:text-foreground"
      viewBox="0 0 24 24"
      fill="currentColor"
      aria-hidden="true"
      focusable="false">
      <path
        d="M8.691 2.188C3.891 2.188 0 5.476 0 9.53c0 2.212 1.17 4.203 3.002 5.55a.59.59 0 0 1 .213.665l-.39 1.48c-.019.07-.048.141-.048.213 0 .163.13.295.29.295a.326.326 0 0 0 .167-.054l1.903-1.114a.864.864 0 0 1 .717-.098 10.16 10.16 0 0 0 2.837.403c.276 0 .543-.027.811-.05-.857-2.578.157-4.972 1.932-6.446 1.703-1.415 3.882-1.98 5.853-1.838-.576-3.583-4.196-6.348-8.596-6.348zM5.785 5.991c.642 0 1.162.529 1.162 1.18a1.17 1.17 0 0 1-1.162 1.178A1.17 1.17 0 0 1 4.623 7.17c0-.651.52-1.18 1.162-1.18zm5.813 0c.642 0 1.162.529 1.162 1.18a1.17 1.17 0 0 1-1.162 1.178 1.17 1.17 0 0 1-1.162-1.178c0-.651.52-1.18 1.162-1.18zm5.34 2.867c-1.797-.052-3.746.512-5.28 1.786-1.72 1.428-2.687 3.72-1.78 6.22.942 2.453 3.666 4.229 6.884 4.229.826 0 1.622-.12 2.361-.336a.722.722 0 0 1 .598.082l1.584.926a.272.272 0 0 0 .14.047c.134 0 .24-.111.24-.247 0-.06-.023-.12-.038-.177l-.327-1.233a.582.582 0 0 1-.023-.156.49.49 0 0 1 .201-.398C23.024 18.48 24 16.82 24 14.98c0-3.21-2.931-5.837-6.656-6.088V8.89c-.135-.01-.27-.027-.407-.03zm-2.53 3.274c.535 0 .969.44.969.982a.976.976 0 0 1-.969.983.976.976 0 0 1-.969-.983c0-.542.434-.982.97-.982zm4.844 0c.535 0 .969.44.969.982a.976.976 0 0 1-.969.983.976.976 0 0 1-.969-.983c0-.542.434-.982.969-.982z" />
    </svg>
  {:else}
    <!-- Simple Icons (CC0) monochrome path so the mark follows theme currentColor. -->
    <svg
      data-icon="inline-start"
      class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
      viewBox="0 0 24 24"
      fill="currentColor"
      aria-hidden="true"
      focusable="false">
      <path
        d="M17.813 4.653h.854c1.51.054 2.769.578 3.773 1.574 1.004.995 1.524 2.249 1.56 3.76v7.36c-.036 1.51-.556 2.769-1.56 3.773s-2.262 1.524-3.773 1.56H5.333c-1.51-.036-2.769-.556-3.773-1.56S.036 18.858 0 17.347v-7.36c.036-1.511.556-2.765 1.56-3.76 1.004-.996 2.262-1.52 3.773-1.574h.774l-1.174-1.12a1.234 1.234 0 0 1-.373-.906c0-.356.124-.658.373-.907l.027-.027c.267-.249.573-.373.92-.373.347 0 .653.124.92.373L9.653 4.44c.071.071.134.142.187.213h4.267a.836.836 0 0 1 .16-.213l2.853-2.747c.267-.249.573-.373.92-.373.347 0 .662.151.929.4.267.249.391.551.391.907 0 .355-.124.657-.373.906zM5.333 7.24c-.746.018-1.373.276-1.88.773-.506.498-.769 1.13-.786 1.894v7.52c.017.764.28 1.395.786 1.893.507.498 1.134.756 1.88.773h13.334c.746-.017 1.373-.275 1.88-.773.506-.498.769-1.129.786-1.893v-7.52c-.017-.765-.28-1.396-.786-1.894-.507-.497-1.134-.755-1.88-.773zM8 11.107c.373 0 .684.124.933.373.25.249.383.569.4.96v1.173c-.017.391-.15.711-.4.96-.249.25-.56.374-.933.374s-.684-.125-.933-.374c-.25-.249-.383-.569-.4-.96V12.44c0-.373.129-.689.386-.947.258-.257.574-.386.947-.386zm8 0c.373 0 .684.124.933.373.25.249.383.569.4.96v1.173c-.017.391-.15.711-.4.96-.249.25-.56.374-.933.374s-.684-.125-.933-.374c-.25-.249-.383-.569-.4-.96V12.44c0-.373.129-.689.386-.947.258-.257.574-.386.947-.386Z" />
    </svg>
  {/if}
{/snippet}

<SettingsSection id="about" title={m.settings_about()}>
  <div class="min-w-0 divide-y divide-border/60">
    <!-- 第 1 行：品牌标识与发行版 -->
    <div class="flex min-h-11 flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 py-3">
      <div class="flex min-w-0 items-center gap-2.5">
        <BrandMark class="size-6" />
        <BrandWordmark class="text-base text-foreground" />
      </div>

      <div class="flex flex-wrap items-center gap-2">
        <span class="text-xs text-muted-foreground">{m.settings_about_edition()}</span>
        {#if editionLabel}
          {#if isCommercial}
            <Badge
              variant="outline"
              class="border-primary/25 bg-primary/10 text-primary dark:border-primary/35 dark:bg-primary/15">
              {editionLabel}
            </Badge>
          {:else}
            <Badge variant="secondary">{editionLabel}</Badge>
          {/if}
          {#if licenseNotice}
            <StatusIndicator compact label={licenseNotice.label} tone={licenseNotice.tone} />
          {/if}
        {:else}
          <span class="font-technical text-xs text-muted-foreground">–</span>
        {/if}
      </div>
    </div>

    <!-- 第 2 行：官方链接与社区入口 -->
    <div class="flex min-h-11 flex-wrap items-center justify-center gap-2 px-4 py-3">
      <Button
        type="button"
        variant="ghost"
        size="sm"
        title={websiteUrl}
        class="gap-1.5"
        onclick={() => void openExternalUrl(websiteUrl)}>
        <GlobeIcon
          data-icon="inline-start"
          class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
          aria-hidden="true" />
        <span>{m.settings_about_website()}</span>
        <ArrowUpRightIcon
          data-icon="inline-end"
          class="text-muted-foreground/80 transition-[color,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:translate-x-px group-hover/button:-translate-y-px group-hover/button:text-foreground"
          aria-hidden="true" />
      </Button>

      <Button
        type="button"
        variant="ghost"
        size="sm"
        title={docsUrl}
        class="gap-1.5"
        onclick={() => void openExternalUrl(docsUrl)}>
        <BookOpenIcon
          data-icon="inline-start"
          class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
          aria-hidden="true" />
        <span>{m.settings_about_docs()}</span>
        <ArrowUpRightIcon
          data-icon="inline-end"
          class="text-muted-foreground/80 transition-[color,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:translate-x-px group-hover/button:-translate-y-px group-hover/button:text-foreground"
          aria-hidden="true" />
      </Button>

      <Button
        type="button"
        variant="ghost"
        size="sm"
        title={`${m.settings_about_github_star()} · ${githubUrl}`}
        aria-label={`${m.settings_about_github()} — ${m.settings_about_github_star()}`}
        class="gap-1.5"
        onclick={() => void openExternalUrl(githubUrl)}>
        {@render brandIcon('github')}
        <span>{m.settings_about_github()}</span>
        <StarIcon
          data-icon="inline-end"
          class="fill-warning/20 text-warning transition-[fill] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:fill-warning"
          aria-hidden="true" />
      </Button>

      <Button
        type="button"
        variant="ghost"
        size="sm"
        title={bilibiliUrl}
        class="gap-1.5"
        onclick={() => void openExternalUrl(bilibiliUrl)}>
        {@render brandIcon('bilibili')}
        <span>{m.settings_about_bilibili()}</span>
        <ArrowUpRightIcon
          data-icon="inline-end"
          class="text-muted-foreground/80 transition-[color,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:translate-x-px group-hover/button:-translate-y-px group-hover/button:text-foreground"
          aria-hidden="true" />
      </Button>

      <Button
        type="button"
        variant="ghost"
        size="sm"
        title={m.settings_about_qq_copy_number()}
        aria-label={`${m.settings_about_qq_group()} ${qqGroupNumber} — ${m.settings_about_qq_copy_number()}`}
        class="max-w-full gap-1.5"
        onclick={handleCopyQqGroup}>
        <MessagesSquareIcon
          data-icon="inline-start"
          class="text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
          aria-hidden="true" />
        <span>{m.settings_about_qq_group()}</span>
        <span
          class={[
            'font-technical text-xs font-normal tabular-nums transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)]',
            copiedTarget === 'qq' ? 'text-signal' : 'text-muted-foreground group-hover/button:text-foreground',
          ]}>
          {qqGroupNumber}
        </span>
        {#if copiedTarget === 'qq'}
          <CheckIcon data-icon="inline-end" class="text-signal" aria-hidden="true" />
        {:else}
          <CopyIcon
            data-icon="inline-end"
            class="text-muted-foreground/80 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground"
            aria-hidden="true" />
        {/if}
      </Button>

      <Popover.Root>
        <Popover.Trigger>
          {#snippet child({ props })}
            <Button
              {...props}
              type="button"
              variant="ghost"
              size="sm"
              title={m.settings_about_wechat_title()}
              class="gap-1.5">
              {@render brandIcon('wechat')}
              <span>{m.settings_about_wechat()}</span>
              <QrCodeIcon
                data-icon="inline-end"
                class="text-muted-foreground/80 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-hover/button:text-foreground group-aria-expanded/button:text-foreground"
                aria-hidden="true" />
            </Button>
          {/snippet}
        </Popover.Trigger>
        <Popover.Content
          align="end"
          class="w-60 max-w-[calc(100vw-2rem)] gap-3 p-3.5"
          role="dialog"
          aria-labelledby={wechatTitleId}>
          <Popover.Header>
            <Popover.Title id={wechatTitleId}>{m.settings_about_wechat_title()}</Popover.Title>
            <Popover.Description class="text-xs">{m.settings_about_wechat_description()}</Popover.Description>
          </Popover.Header>
          <div class="mx-auto inline-flex rounded-lg border border-border bg-white p-2 shadow-2xs dark:border-white/15">
            <img
              src={wechatOfficialQr}
              alt={m.settings_about_wechat_qr_alt()}
              width="258"
              height="258"
              decoding="async"
              class="block size-40 rounded-xs object-contain" />
          </div>
          <div class="flex items-center justify-between gap-2 border-t border-border/60 pt-2.5">
            <span
              class={[
                'truncate text-sm font-medium transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] select-all',
                copiedTarget === 'wechat' ? 'text-signal' : 'text-foreground',
              ]}>
              {wechatAccountName}
            </span>
            <Button type="button" variant="outline" size="sm" onclick={handleCopyWechatName}>
              {#if copiedTarget === 'wechat'}
                <CheckIcon data-icon="inline-start" class="text-signal" />
                {m.settings_about_copied()}
              {:else}
                <CopyIcon data-icon="inline-start" />
                {m.settings_about_wechat_copy_name()}
              {/if}
            </Button>
          </div>
        </Popover.Content>
      </Popover.Root>
    </div>
  </div>
</SettingsSection>
