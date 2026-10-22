<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import type { WebSearchBackend, WebSearchConfig, WebSearchConfigView } from '#lib/types.js'
import InlineSettingInput from '#lib/components/inline-setting-input.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import WebAccessConfiguration from '#lib/components/web-access-configuration.svelte'
import WebSearchBindingEditor from '#lib/components/web-search-binding-editor.svelte'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as Collapsible from '#lib/components/ui/collapsible/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

const queryClient = useQueryClient()
const configQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const eligibleModelsQuery = createQuery(() => ({
  queryKey: ['web-search-eligible-models'],
  queryFn: admin.webSearch.eligibleModels,
}))
const externalRoutesQuery = createQuery(() => ({
  queryKey: ['web-search-external-routes'],
  queryFn: admin.webSearch.externalRoutes,
}))

// Local 来源与预算只属于已保存的 Local 绑定；尚未配置绑定时按 Local 呈现。
const usesLocal = $derived(configQuery.data !== undefined && configQuery.data.backend?.kind !== 'external')

const sourceSettingsQuery = createQuery(() => ({
  queryKey: ['web-access-settings'],
  queryFn: admin.webAccess.settings.get,
  enabled: usesLocal,
}))
const sourceProvidersQuery = createQuery(() => ({
  queryKey: ['web-providers'],
  queryFn: admin.webAccess.providers.list,
  enabled: usesLocal,
}))
const sourceBrowserQuery = createQuery(() => ({
  queryKey: ['web-access-browser'],
  queryFn: admin.webAccess.browser.get,
  enabled: usesLocal,
}))

let toggleSaving = $state(false)
let toggleError = $state('')
// 任一完整配置写入进行中：总开关、预算行与绑定 Sheet 串行，避免用同一 revision 并发提交。
let writing = $state(false)
let editorOpen = $state(false)
let advancedOpen = $state(false)

const config = $derived(configQuery.data)
const limits = $derived(config?.limits)
const eligibleModels = $derived(eligibleModelsQuery.data ?? [])
const externalRoutes = $derived(externalRoutesQuery.data ?? [])
const backend = $derived(config?.backend)

const methodLabel = $derived(
  backend?.kind === 'external' ? m.web_search_use_external_route() : m.web_search_use_stravia_model(),
)
// 已保存的目标名称；不可用或已移除的历史值仍以原始标识呈现。
const bindingTarget = $derived.by(() => {
  if (backend?.kind === 'external') {
    if (!backend.route_id) return m.web_search_not_selected()
    return externalRoutes.find((route) => route.model_id === backend.route_id)?.display_name ?? backend.route_id
  }
  if (!backend?.model_id) return m.web_search_not_selected()
  return eligibleModels.find((model) => model.id === backend.model_id)?.display_name ?? backend.model_id
})

// 读取失败与缺少前置条件是两种状态：失败给重试，缺失给补齐动作。
const bindingIssue = $derived.by((): 'loading' | 'unavailable' | 'missing' | undefined => {
  if (!backend) return 'missing'
  if (backend.kind === 'local') {
    if (eligibleModelsQuery.isError) return 'unavailable'
    if (eligibleModelsQuery.isPending) return 'loading'
    return eligibleModels.some((model) => model.id === backend.model_id) ? undefined : 'missing'
  }
  if (externalRoutesQuery.isError) return 'unavailable'
  if (externalRoutesQuery.isPending) return 'loading'
  return externalRoutes.some((route) => route.model_id === backend.route_id && route.available) ? undefined : 'missing'
})
const savedBindingReady = $derived(bindingIssue === undefined)

// Availability combines server capability metadata with browser status; core validates activation.
const savedSourceIssue = $derived.by(() => {
  if (configQuery.data?.backend?.kind !== 'local') return undefined
  // 在提前返回前订阅全部依赖，避免某项加载失败时其余查询停留在旧快照。
  const settingsStatus = sourceSettingsQuery.status
  const providersStatus = sourceProvidersQuery.status
  const settings = sourceSettingsQuery.data
  const providers = sourceProvidersQuery.data ?? []
  const browserStatus = sourceBrowserQuery.status
  const browserFetching = sourceBrowserQuery.isFetching
  const browserAvailable = sourceBrowserQuery.data?.available === true
  if (settingsStatus === 'error' || providersStatus === 'error') return 'unavailable'
  if (settingsStatus === 'pending' || providersStatus === 'pending') return 'loading'
  const hasSource = (capability: 'search' | 'fetch', ids: string[]) =>
    providers.some(
      (provider) =>
        ids.includes(provider.id) &&
        provider.capabilities[capability] &&
        (provider.kind !== 'local' || (browserStatus === 'success' && !browserFetching && browserAvailable)),
    )
  if (
    settings &&
    hasSource('search', settings.search_provider_ids) &&
    hasSource('fetch', settings.fetch_provider_ids)
  ) {
    return undefined
  }
  const selectedLocal = providers.some(
    (provider) =>
      provider.kind === 'local' &&
      (settings?.search_provider_ids.includes(provider.id) || settings?.fetch_provider_ids.includes(provider.id)),
  )
  if (selectedLocal && browserFetching) return 'loading'
  if (selectedLocal && browserStatus === 'error') return 'unavailable'
  return 'missing'
})
const canEnable = $derived(savedBindingReady && !savedSourceIssue)

// 已保存的预算超出支持范围时展开高级选项，让行内错误可见。
$effect(() => {
  if (config && limits && usesLocal) {
    const turnsOk = config.max_turns >= limits.min_turns && config.max_turns <= limits.max_turns
    const timeOk =
      config.total_time_seconds >= limits.min_total_time_seconds &&
      config.total_time_seconds <= limits.max_total_time_seconds
    if (!turnsOk || !timeOk) advancedOpen = true
  }
})

interface ConfigPatch {
  enabled?: boolean
  backend?: WebSearchBackend
  max_turns?: number
  total_time_seconds?: number
}

/**
 * 完整配置写入：始终以提交瞬间已确认的配置为底，只覆盖本次要改的字段。
 * 因此不会携带 Sheet 或另一行的草稿，也不会用旧快照覆盖其他表面刚确认的值；
 * 失败（含 revision 冲突）时重新读取确认值，调用方保留各自的输入。
 */
async function writeConfig(patch: ConfigPatch): Promise<WebSearchConfigView> {
  const current = configQuery.data
  if (!current) throw new Error(m.web_search_settings_not_loaded())
  writing = true
  try {
    const input: WebSearchConfig = {
      revision: current.revision,
      enabled: patch.enabled ?? current.enabled,
      backend: patch.backend ?? current.backend,
      max_turns: patch.max_turns ?? current.max_turns,
      total_time_seconds: patch.total_time_seconds ?? current.total_time_seconds,
      updated_at: current.updated_at,
    }
    const saved = await admin.webSearch.config.update(input)
    queryClient.setQueryData(['web-search-config'], saved)
    return saved
  } catch (error) {
    void queryClient.invalidateQueries({ queryKey: ['web-search-config'] })
    throw error
  } finally {
    writing = false
  }
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data
  if (!current || writing || enabled === current.enabled || (enabled && !canEnable)) return
  toggleSaving = true
  toggleError = ''
  try {
    await writeConfig({ enabled })
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function saveBinding(next: WebSearchBackend): Promise<void> {
  await writeConfig({ backend: next })
}

async function saveMaxTurns(next: string): Promise<string> {
  return String((await writeConfig({ max_turns: Number(next) })).max_turns)
}

async function saveTotalSeconds(next: string): Promise<string> {
  return String((await writeConfig({ total_time_seconds: Number(next) })).total_time_seconds)
}

function rangeValidator(min: number, max: number): (draft: string) => string | null {
  return (draft) => {
    const value = Number(draft)
    return draft.trim() !== '' && Number.isInteger(value) && value >= min && value <= max
      ? null
      : m.web_search_choose_supported_limits()
  }
}
</script>

<svelte:head><title>{m.web_search_title()} · Stravia</title></svelte:head>

<div class="route-page">
  <PageHeader title={m.web_search_title()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.web_search_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {/if}
  {#if configQuery.isPending}
    <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
  {/if}
  {#if config}
    <div class="content-column flex flex-col gap-8">
      <div class="flex flex-col gap-3">
        <SettingRow title={m.web_search_enable()} layout="inline">
          {#snippet children({ titleId })}
            {#if toggleSaving}<Spinner aria-hidden="true" />{/if}
            <Switch
              bind:checked={() => configQuery.data?.enabled ?? false, (value: boolean) => void toggleEnabled(value)}
              disabled={writing || (!(configQuery.data?.enabled ?? false) && !canEnable)}
              aria-busy={toggleSaving}
              aria-labelledby={titleId} />
          {/snippet}
        </SettingRow>
        {#if toggleError}
          <Alert.Root variant="destructive"><Alert.Description>{toggleError}</Alert.Description></Alert.Root>
        {/if}
        {#if bindingIssue === 'unavailable'}
          <RequestFailure
            title={backend?.kind === 'external'
              ? m.web_search_external_routes_not_loaded()
              : m.web_search_settings_not_loaded()}
            message={localizeBackendErrorMessage(
              backend?.kind === 'external' ? externalRoutesQuery.error : eligibleModelsQuery.error,
            )}
            retry={() => (backend?.kind === 'external' ? externalRoutesQuery.refetch() : eligibleModelsQuery.refetch())}
            retrying={backend?.kind === 'external' ? externalRoutesQuery.isFetching : eligibleModelsQuery.isFetching} />
        {:else if bindingIssue === 'missing'}
          <Alert.Root variant="warning" role="status">
            <Alert.Description>
              {m.web_search_enable_prerequisite()}
              <Button variant="link" size="sm" disabled={writing} onclick={() => (editorOpen = true)}
                >{m.web_search_edit_method()}</Button>
            </Alert.Description>
          </Alert.Root>
        {/if}
        {#if savedSourceIssue}
          <Alert.Root variant="warning" role="status">
            <Alert.Description>
              {savedSourceIssue === 'loading'
                ? m.common_settings_loading()
                : savedSourceIssue === 'unavailable'
                  ? m.web_search_sources_unavailable()
                  : m.web_search_sources_required()}
              <Button
                variant="link"
                size="sm"
                onclick={() => document.getElementById('web-search-sources')?.scrollIntoView({ block: 'start' })}
                >{m.web_search_configure_sources()}</Button>
            </Alert.Description>
          </Alert.Root>
        {/if}
      </div>

      <SettingsSection id="web-search-method" title={m.web_search_method_title()}>
        {#snippet action()}
          <Button
            variant="outline"
            size="sm"
            disabled={writing}
            aria-label={m.web_search_edit_method()}
            onclick={() => (editorOpen = true)}>{m.common_edit()}</Button>
        {/snippet}
        <SettingRow title={m.web_search_method()} value={methodLabel} />
        <SettingRow
          title={backend?.kind === 'external' ? m.web_search_external_route() : m.web_search_model_used()}
          value={bindingTarget} />
      </SettingsSection>

      {#if usesLocal}
        <Collapsible.Root bind:open={advancedOpen} class="flex flex-col gap-4">
          <Collapsible.Trigger class={buttonVariants({ variant: 'outline', class: 'self-start' })}
            >{m.common_advanced()}</Collapsible.Trigger>
          <Collapsible.Content>
            <SettingsSection id="web-search-advanced-fields" title={m.web_search_limits_title()}>
              {#if limits}
                <SettingRow
                  title={m.web_search_maximum_steps()}
                  labelFor="search-max-turns"
                  controlSize="number"
                  hint="{m.web_search_limits_help()} {m.web_search_range_hint({
                    min: limits.min_turns,
                    max: limits.max_turns,
                  })}">
                  {#snippet children({ titleId })}
                    <InlineSettingInput
                      id="search-max-turns"
                      labelledby={titleId}
                      type="number"
                      inputmode="numeric"
                      min={limits.min_turns}
                      max={limits.max_turns}
                      disabled={writing}
                      value={String(config.max_turns)}
                      validate={rangeValidator(limits.min_turns, limits.max_turns)}
                      onSave={saveMaxTurns} />
                  {/snippet}
                </SettingRow>
                <SettingRow
                  title={m.web_search_time_limit_seconds()}
                  labelFor="search-total-seconds"
                  hint="{m.web_search_limits_help()} {m.web_search_range_hint({
                    min: limits.min_total_time_seconds,
                    max: limits.max_total_time_seconds,
                  })}">
                  {#snippet children({ titleId })}
                    <InlineSettingInput
                      id="search-total-seconds"
                      labelledby={titleId}
                      type="number"
                      inputmode="numeric"
                      min={limits.min_total_time_seconds}
                      max={limits.max_total_time_seconds}
                      suffix={m.web_search_seconds_unit()}
                      disabled={writing}
                      value={String(config.total_time_seconds)}
                      validate={rangeValidator(limits.min_total_time_seconds, limits.max_total_time_seconds)}
                      onSave={saveTotalSeconds} />
                  {/snippet}
                </SettingRow>
              {/if}
            </SettingsSection>
          </Collapsible.Content>
        </Collapsible.Root>

        <WebAccessConfiguration />
      {/if}
    </div>
  {/if}
</div>

<WebSearchBindingEditor
  bind:open={editorOpen}
  {backend}
  eligibleModels={eligibleModelsQuery}
  externalRoutes={externalRoutesQuery}
  onsave={saveBinding} />
