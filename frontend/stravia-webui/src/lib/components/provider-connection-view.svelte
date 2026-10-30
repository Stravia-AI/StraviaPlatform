<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { onDestroy, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { admin, currencyQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { formatLogTime } from '#lib/format.js'
import { localeState } from '#lib/localization.svelte.js'
import { currencyForKind, formatCostPlanPrice } from '#lib/money.js'
import { resolvePluginText } from '#lib/plugin-text.js'
import {
  costDraftFromPlan,
  costDraftIssue,
  costDraftMatchesPlan,
  costPlanInput,
  type ProviderCostDraftFields,
} from '#lib/provider-cost-draft.js'
import {
  mergeProviderValidationIssues,
  providerConnectionCheckBaseUrlIssues,
  providerConnectionCheckConfigurationIssues,
  providerConnectionCheckFailureMessage,
  providerConnectionCheckFieldIssues,
  providerConnectionCheckSuccessMessage,
  providerConnectionCheckUnsupportedMessage,
} from '#lib/provider-connection-check.js'
import { ProviderConnectionDraft, type ProviderConnectionSubmission } from '#lib/provider-connection-draft.svelte.js'
import type {
  CostBasis,
  CurrencyKind,
  Provider,
  ProviderCostPlan,
  ProviderValidationIssue,
  SubscriptionPeriod,
  UpdateProvider,
} from '#lib/types.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import FormSection from '#lib/components/form-section.svelte'
import ProviderConfigFields from '#lib/components/provider-config-fields.svelte'
import ProviderConnectionStatus from '#lib/components/provider-connection-status.svelte'
import ProviderOAuthAuthorization from '#lib/components/provider-oauth-authorization.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import SettingRow from '#lib/components/setting-row.svelte'
import { Switch } from '#lib/components/ui/switch/index.js'

interface Props {
  provider: Provider
  onSaved?: (provider: Provider) => void
}

let { provider, onSaved }: Props = $props()
const initialProvider = untrack(() => provider)
const queryClient = useQueryClient()
let testing = $state(false)
let connectionError = $state('')
let connectionNotice = $state('')
let testIssues = $state<ProviderValidationIssue[]>([])
let testGeneration = 0

let oauthAuthorization = $state<{
  cancel: () => Promise<void>
  consume: () => void
  updateProxy: (useProxy: boolean) => Promise<void>
}>()

const providerDescriptorsQuery = createQuery(() => ({
  queryKey: ['provider-descriptors'],
  queryFn: admin.providers.descriptors,
}))

const vendorId = $derived(provider.vendor ?? '')
const descriptor = $derived(providerDescriptorsQuery.data?.find((item) => item.provider_id === vendorId))
const channel = $derived(descriptor?.channels.find((item) => item.id === provider.channel))
const configFields = $derived(descriptor?.config_fields ?? [])
const configuredSecretFields = $derived(provider.configured_credential_fields ?? [])
const oauthProvider = $derived(Boolean(channel?.auth))
const supportsDiscovery = $derived(channel?.capabilities.includes('model_discovery') ?? false)
const supportsConfigValidation = $derived(channel?.capabilities.includes('config_validation') ?? false)

// 成本草稿与连接字段同属一份表单：成本查询只用于历史与当前生效记录，
// 编辑状态从 Provider 详情里的当前记录起步，不另开编辑表面。
const costPlansQuery = createQuery(() => ({
  queryKey: ['provider-cost-plans', provider.id],
  queryFn: () => admin.providers.costPlans(provider.id),
}))
const currencyQuery = createQuery(() => currencyQueryOptions())
const customCurrency = $derived(currencyQuery.data ?? null)
const costPlans = $derived(costPlansQuery.data ?? [])
const effectiveCostPlan = $derived(costPlans[0] ?? provider.cost_plan ?? null)
let costFields = $state<ProviderCostDraftFields>(costDraftFromPlan(untrack(() => provider.cost_plan)))
let costTouched = $state(false)
let costSaving = $state(false)
let costError = $state('')
let costSaveAttempted = $state(false)
const costDirty = $derived(!costDraftMatchesPlan(costFields, effectiveCostPlan))
const costIssue = $derived(costDraftIssue(costFields))
const showCostIssue = $derived(Boolean(costIssue) && (costFields.amount.length > 0 || costSaveAttempted))

// 历史记录晚于 Provider 详情到达时，未编辑过的草稿跟随最新生效记录，避免表单凭空变脏。
$effect(() => {
  const plan = costPlansQuery.data?.[0]
  if (!plan || costTouched) return
  untrack(() => {
    costFields = costDraftFromPlan(plan)
  })
})

function updateProvider(submission: ProviderConnectionSubmission): Promise<Provider> {
  const input: UpdateProvider = {
    name: submission.name,
    base_url: submission.baseUrl,
    use_proxy: submission.useProxy,
    vendor_options: submission.options,
    ...(Object.keys(submission.credentials).length > 0 ? { adapter_credentials: submission.credentials } : {}),
  }

  return admin.providers.update(provider.id, input)
}

const draft = new ProviderConnectionDraft({
  fields: {
    name: initialProvider.name,
    baseUrl: initialProvider.base_url,
    protocol: initialProvider.protocol,
    useProxy: initialProvider.use_proxy,
    values: { ...(initialProvider.vendor_options ?? {}) },
  },

  target: () => (descriptor && channel ? { kind: 'edit', provider, descriptor, channel } : undefined),
  api: admin.providers,
  writer: { write: updateProvider, bind: (saved, sessionId) => admin.providers.bindOAuth(saved.id, sessionId) },
  oauth: {
    cancel: () => oauthAuthorization?.cancel() ?? Promise.resolve(),
    consume: () => oauthAuthorization?.consume(),
    updateProxy: (useProxy: boolean) => oauthAuthorization?.updateProxy(useProxy) ?? Promise.resolve(),
  },
  hooks: {
    onSaved: async (saved) => {
      baseline.reset()
      invalidateConnectionCheck()
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['providers'] }),
        queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
      ])
      // 连接保存成功后再追加成本记录；追加失败只留下成本草稿与错误，不回滚已保存的连接。
      const savingCost = costDirty
      const costSaved = await appendCostPlanIfChanged()
      toast.success(
        savingCost && costSaved
          ? m.provider_connection_view_connection_and_cost_saved()
          : m.provider_connection_view_connection_settings_saved(),
      )
      onSaved?.(saved)
    },
    onPartialSave: (saved) => {
      baseline.reset()
      invalidateConnectionCheck()
      void Promise.all([
        queryClient.invalidateQueries({ queryKey: ['providers'] }),
        queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
      ]).catch((error) => toast.error(localizeBackendErrorMessage(error)))
      void appendCostPlanIfChanged()

      onSaved?.(saved)
    },
  },
})

const previewIssues = $derived(draft.preview?.issues ?? [])
const previewIssueError = $derived(
  previewIssues
    .filter((issue) => !issue.field)
    .map((issue) => resolvePluginText(issue.message, localeState.current))
    .join(' ') || (previewIssues.length > 0 ? m.provider_config_fix_issues_before_save() : ''),
)

const connectionIssues = $derived(mergeProviderValidationIssues(previewIssues, testIssues))
const baseUrlIssues = $derived(providerConnectionCheckBaseUrlIssues(connectionIssues))
const configurationIssues = $derived(providerConnectionCheckConfigurationIssues(connectionIssues))

const submitMessage = $derived(
  draft.partialSaveError !== undefined
    ? m.provider_config_saved_auth_bind_failed({ error: localizeBackendErrorMessage(draft.partialSaveError) })
    : draft.submitError !== undefined
      ? localizeBackendErrorMessage(draft.submitError)
      : '',
)

// 连接与成本同属一份表单：任一草稿有未保存变更都触发离开确认，提交期间一起锁定。
const formError = $derived([connectionError, submitMessage, previewIssueError, costError].filter(Boolean).join(' '))
const baseline = new DraftBaseline(() => draft.fields)
const busy = $derived(draft.submitting || costSaving)
const dirty = $derived(baseline.dirty || costDirty)
// 只有成本变化时不需要连接可提交；连接有改动时仍按原有连接校验决定。
const canSave = $derived(!costIssue && (draft.submittable || (costDirty && !baseline.dirty)))
const guard = new DraftGuard({ dirty: () => dirty, busy: () => busy })

onDestroy(() => {
  invalidateConnectionCheck()
  draft.dispose()
})

$effect(() => {
  if (!descriptor) return
  const additions: Record<string, unknown> = {}
  for (const field of descriptor.config_fields) {
    if (!field.secret && !(field.key in draft.fields.values) && field.default_json != null) {
      additions[field.key] = field.default_json
    } else if (!field.secret && !(field.key in draft.fields.values) && field.required && field.kind.type === 'bool') {
      additions[field.key] = false
    }
  }
  if (Object.keys(additions).length > 0) {
    // 补齐默认值不是用户改动：基线干净时一并更新基线。
    const clean = untrack(() => !baseline.dirty)
    draft.fields.values = { ...additions, ...draft.fields.values }
    if (clean) untrack(() => baseline.reset())
  }
})

/**
 * 草稿、目标或 OAuth 快照变化后，进行中的检查结果不得再写回当前界面。
 * 递增代际同时收起旧反馈，允许用户立即用新草稿重新检查。
 */
function invalidateConnectionCheck(): void {
  testGeneration += 1
  testing = false
  testIssues = []
  connectionError = ''
  connectionNotice = ''
}

$effect(() => {
  void provider.id
  void provider.updated_at
  void descriptor
  untrack(invalidateConnectionCheck)
})

// 取消：把连接与成本草稿都还原为已保存状态，并放弃进行中的授权；有改动时先经过离开确认。
function revert(): void {
  invalidateConnectionCheck()
  costError = ''
  costSaveAttempted = false
  costTouched = false
  costFields = costDraftFromPlan(effectiveCostPlan)
  void draft
    .resetConnection({
      name: provider.name,
      baseUrl: provider.base_url,
      protocol: provider.protocol,
      useProxy: provider.use_proxy,
      values: { ...(provider.vendor_options ?? {}) },
    })
    .then(() => baseline.reset())
}

function save(): void {
  invalidateConnectionCheck()
  costError = ''
  if (busy) return
  if (costIssue) {
    costSaveAttempted = true
    return
  }
  // 只有成本草稿变化时，不重跑连接预览与 PUT，直接追加生效记录。
  if (!baseline.dirty && costDirty) {
    void appendCostPlanIfChanged().then((saved) => {
      if (saved) toast.success(m.cost_plan_saved())
    })
    return
  }
  void draft.submit()
}

function costBasisLabel(basis: CostBasis): string {
  if (basis === 'none') return m.cost_basis_none()
  if (basis === 'subscription') return m.cost_basis_subscription()
  return m.cost_basis_usage()
}

function costBasisHelp(basis: CostBasis): string {
  if (basis === 'none') return m.cost_basis_none_help()
  if (basis === 'subscription') return m.cost_basis_subscription_help()
  return m.cost_basis_usage_help()
}

function planSummary(plan: ProviderCostPlan | null): string {
  if (!plan || plan.basis === 'usage') return m.cost_basis_usage()
  if (plan.basis === 'none') return m.cost_basis_none()
  const periodLabel = plan.period === 'year' ? m.cost_period_year() : m.cost_period_month()
  return `${periodLabel} · ${formatCostPlanPrice(plan.price, customCurrency)}`
}

function costChanged(): void {
  costTouched = true
  costError = ''
  costSaveAttempted = false
}

function setCostBasis(value: string): void {
  if (!value || value === costFields.basis) return
  const previous = costFields.basis
  costFields.basis = value as CostBasis
  // 新建订阅价格时沿用展示币种（与旧编辑器一致）；已有价格写法保持不变。
  if (value === 'subscription' && previous !== 'subscription' && !costFields.amount) {
    costFields.currency = customCurrency ? 'custom' : 'usd'
  }
  costChanged()
}

function setCostPeriod(value: string): void {
  if (!value || value === costFields.period) return
  costFields.period = value as SubscriptionPeriod
  costChanged()
}

function setCostCurrency(value: string): void {
  if (!value || value === costFields.currency) return
  costFields.currency = value as CurrencyKind
  costChanged()
}

/**
 * 追加成本记录。与生效记录相同时不发请求；失败时保留草稿与错误，
 * 连接保存已经成功的事实不受影响。返回值表示成本记录是否已与草稿一致。
 */
async function appendCostPlanIfChanged(): Promise<boolean> {
  if (costSaving) return false
  if (!costDirty) return true
  const input = costPlanInput(costFields)
  if (!input) {
    costSaveAttempted = true
    return false
  }
  costSaving = true
  costError = ''
  try {
    const plan = await admin.providers.appendCostPlan(provider.id, input)
    queryClient.setQueryData<ProviderCostPlan[]>(['provider-cost-plans', provider.id], (current) => [
      plan,
      ...(current ?? []),
    ])
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['provider-cost-plans', provider.id] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-providers'] }),
    ])
    // 以服务端返回的记录重置草稿，避免金额写法被规范化后仍被判为未保存。
    costFields = costDraftFromPlan(plan)
    costTouched = false
    costSaveAttempted = false
    return true
  } catch (error) {
    costError = localizeBackendErrorMessage(error)
    // 连接可能已经保存：刷新 Provider 数据，成本草稿保持未保存，错误留在表单里。
    await queryClient.invalidateQueries({ queryKey: ['providers'] })
    return false
  } finally {
    costSaving = false
  }
}

function nameChanged(): void {
  invalidateConnectionCheck()
  draft.nameChanged()
}

function configurationChanged(): void {
  invalidateConnectionCheck()
  draft.configurationChanged()
}

function useProxyChanged(checked: boolean): void {
  invalidateConnectionCheck()
  draft.fields.useProxy = checked
  draft.useProxyChanged()
}

function oauthStateChanged(sessionId: string | undefined, ready: boolean): void {
  invalidateConnectionCheck()
  draft.oauthStateChanged(sessionId, ready)
}

async function testConnection(): Promise<void> {
  if (testing || busy || !descriptor || !channel) return
  const input = draft.connectionCheckInput({ kind: 'edit', provider, descriptor, channel })
  invalidateConnectionCheck()
  testing = true
  const generation = testGeneration
  try {
    const result = await admin.providers.test(provider.id, input)
    if (generation !== testGeneration) return
    if (result.status === 'passed') {
      toast.success(providerConnectionCheckSuccessMessage(result))
    } else if (result.status === 'unsupported') {
      connectionNotice = providerConnectionCheckUnsupportedMessage()
    } else if (result.status === 'failed') {
      testIssues = providerConnectionCheckFieldIssues(result)
      connectionError = providerConnectionCheckFailureMessage(result)
    }
  } catch (error) {
    if (generation !== testGeneration) return
    connectionError = localizeBackendErrorMessage(error)
  } finally {
    if (generation === testGeneration) testing = false
  }
}
</script>

<section class="route-section" aria-labelledby="provider-connection-title">
  <!-- 页签已经写明「连接设置」，可见标题只会重复；保留语义标题供辅助技术定位。 -->
  <h2 id="provider-connection-title" class="sr-only">{m.provider_connection_view_connection_settings()}</h2>

  {#if providerDescriptorsQuery.isError}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_plugin_unavailable()}</Alert.Title>
      <Alert.Description>{localizeBackendErrorMessage(providerDescriptorsQuery.error)}</Alert.Description>
    </Alert.Root>
  {:else if !providerDescriptorsQuery.isPending && !descriptor}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_plugin_missing()}</Alert.Title>
      <Alert.Description>{m.provider_config_plugin_missing_help({ vendor: vendorId || '—' })}</Alert.Description>
    </Alert.Root>
  {:else if descriptor && !channel}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_channel_missing()}</Alert.Title>
      <Alert.Description
        >{m.provider_config_channel_missing_help({ channel: provider.channel ?? '—' })}</Alert.Description>
    </Alert.Root>
  {/if}

  <!-- 表单始终占居中的设置列；容器足够宽时辅助栏落在表单右侧的空白里，没有内容就不占位，较窄时排在表单之后。 -->
  <div
    class="mx-auto grid w-full max-w-(--content-column-max-width) gap-6 @min-[96rem]/route-page:max-w-none @min-[96rem]/route-page:grid-cols-[minmax(0,1fr)_minmax(0,var(--content-column-max-width))_minmax(0,1fr)] @min-[96rem]/route-page:items-start @min-[96rem]/route-page:gap-x-12">
    <form
      class="@container/provider-form flex min-w-0 flex-col gap-6 @min-[96rem]/route-page:col-start-2 @min-[96rem]/route-page:row-start-1"
      onsubmit={(event) => {
        event.preventDefault()
        save()
      }}>
      <FormSection title={m.provider_connection_view_section_connection()}>
        <!-- 表单列够宽（≥42rem）时名称与 Base URL 同行：名称取 16rem，地址占余下宽度。 -->
        <Field.Group
          class="@2xl/provider-form:grid @2xl/provider-form:grid-cols-[minmax(0,16rem)_minmax(0,1fr)] @2xl/provider-form:gap-x-6">
          <Field.Field>
            <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
            <Input id="provider-name" bind:value={draft.fields.name} required disabled={busy} oninput={nameChanged} />
          </Field.Field>
          <Field.Field data-invalid={baseUrlIssues.length > 0 || undefined}>
            <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
            <Input
              id="provider-base-url"
              class="font-technical"
              bind:value={draft.fields.baseUrl}
              type="url"
              required={!supportsConfigValidation}
              disabled={busy}
              aria-invalid={baseUrlIssues.length > 0 || undefined}
              oninput={configurationChanged} />
            {#each baseUrlIssues as issue, index (`${issue.field}:${issue.code}:${index}`)}
              <Field.Error>{resolvePluginText(issue.message, localeState.current)}</Field.Error>
            {/each}
          </Field.Field>
          {#if descriptor && channel}
            <ProviderConfigFields
              fields={configFields}
              configGroups={descriptor.config_groups}
              protocol={provider.protocol}
              bind:values={draft.fields.values}
              {configuredSecretFields}
              issues={configurationIssues}
              idPrefix={`provider-config-${provider.id}`}
              vendorName={descriptor.display_name}
              disabled={busy}
              onChanged={configurationChanged} />
          {/if}
        </Field.Group>

        {#if draft.unknownOptionKeys.length > 0}
          <Alert.Root>
            <Alert.Title>{m.provider_config_legacy_values_retained()}</Alert.Title>
            <Alert.Description>
              {m.provider_config_legacy_values_retained_help({ fields: draft.unknownOptionKeys.join(', ') })}
            </Alert.Description>
          </Alert.Root>
        {/if}

        {#if oauthProvider && descriptor && channel}
          <ProviderOAuthAuthorization
            bind:this={oauthAuthorization}
            vendorId={descriptor.provider_id}
            channel={channel.id}
            flow={channel.auth!.flow}
            configuration={draft.oauthConfiguration}
            useProxy={draft.fields.useProxy}
            mode="reconnect"
            disabled={busy}
            onStateChange={oauthStateChanged} />
        {/if}
      </FormSection>

      <FormSection title={m.provider_connection_view_section_network()}>
        <!-- 代理开关属于连接草稿：随「保存连接」一起提交，不即时生效。 -->
        <SettingRow
          title={m.common_use_proxy()}
          labelFor="provider-use-proxy"
          description={m.common_send_requests_service_proxy_configured_settings()}
          layout="inline">
          <Switch
            id="provider-use-proxy"
            checked={draft.fields.useProxy}
            disabled={busy}
            onCheckedChange={useProxyChanged} />
        </SettingRow>
      </FormSection>

      <FormSection title={m.cost_plan_section()}>
        <Field.Group>
          <Field.Field size="setting">
            <Field.Label for="provider-cost-basis" hint={m.cost_plan_help()}>{m.cost_plan_basis()}</Field.Label>
            <Select.Root type="single" value={costFields.basis} onValueChange={setCostBasis} disabled={busy}>
              <Select.Trigger id="provider-cost-basis" class="w-full" aria-label={m.cost_plan_basis()}>
                {costBasisLabel(costFields.basis)}
              </Select.Trigger>
              <Select.Content>
                <Select.Group>
                  <Select.Item value="none">{m.cost_basis_none()}</Select.Item>
                  <Select.Item value="usage">{m.cost_basis_usage()}</Select.Item>
                  <Select.Item value="subscription">{m.cost_basis_subscription()}</Select.Item>
                </Select.Group>
              </Select.Content>
            </Select.Root>
            <Field.Description>{costBasisHelp(costFields.basis)}</Field.Description>
          </Field.Field>

          {#if costFields.basis === 'subscription'}
            <Field.Group
              class="@2xl/provider-form:grid @2xl/provider-form:grid-cols-[minmax(0,16rem)_minmax(0,1fr)] @2xl/provider-form:gap-x-6">
              <Field.Field size="setting">
                <Field.Label for="provider-cost-period">{m.cost_plan_period()}</Field.Label>
                <Select.Root type="single" value={costFields.period} onValueChange={setCostPeriod} disabled={busy}>
                  <Select.Trigger id="provider-cost-period" class="w-full" aria-label={m.cost_plan_period()}>
                    {costFields.period === 'year' ? m.cost_period_year() : m.cost_period_month()}
                  </Select.Trigger>
                  <Select.Content>
                    <Select.Group>
                      <Select.Item value="month">{m.cost_period_month()}</Select.Item>
                      <Select.Item value="year">{m.cost_period_year()}</Select.Item>
                    </Select.Group>
                  </Select.Content>
                </Select.Root>
              </Field.Field>
              <Field.Field data-invalid={showCostIssue || undefined}>
                <Field.Label for="provider-cost-amount">{m.cost_plan_price()}</Field.Label>
                <InputGroup.Root>
                  <InputGroup.Addon align="inline-start">
                    <InputGroup.Text class="font-technical"
                      >{currencyForKind(costFields.currency, customCurrency).symbol}</InputGroup.Text>
                  </InputGroup.Addon>
                  <InputGroup.Input
                    id="provider-cost-amount"
                    bind:value={costFields.amount}
                    inputmode="decimal"
                    class="font-technical"
                    placeholder="0.00"
                    autocomplete="off"
                    disabled={busy}
                    aria-invalid={showCostIssue || undefined}
                    oninput={costChanged} />
                </InputGroup.Root>
                <Field.Description>{m.cost_plan_price_help()}</Field.Description>
                {#if showCostIssue}<Field.Error>{costIssue}</Field.Error>{/if}
              </Field.Field>
              {#if customCurrency}
                <Field.Field size="setting">
                  <Field.Label for="provider-cost-currency">{m.cost_plan_currency()}</Field.Label>
                  <Select.Root
                    type="single"
                    value={costFields.currency}
                    onValueChange={setCostCurrency}
                    disabled={busy}>
                    <Select.Trigger id="provider-cost-currency" class="w-full" aria-label={m.cost_plan_currency()}>
                      {costFields.currency === 'custom' ? customCurrency.symbol : m.pricing_currency_usd()}
                    </Select.Trigger>
                    <Select.Content>
                      <Select.Group>
                        <Select.Item value="usd">{m.pricing_currency_usd()}</Select.Item>
                        <Select.Item value="custom">{customCurrency.symbol}</Select.Item>
                      </Select.Group>
                    </Select.Content>
                  </Select.Root>
                </Field.Field>
              {/if}
            </Field.Group>
          {/if}
        </Field.Group>

        <p class="text-xs leading-normal text-muted-foreground">{m.cost_plan_effective_help()}</p>

        <div class="flex min-w-0 flex-col gap-1">
          <h4 class="text-sm font-medium">{m.cost_plan_history()}</h4>
          {#if costPlansQuery.error && costPlansQuery.data === undefined}
            <RequestFailure
              title={m.cost_plan_not_loaded()}
              message={localizeBackendErrorMessage(costPlansQuery.error)}
              retry={() => void costPlansQuery.refetch()}
              retrying={costPlansQuery.isFetching} />
          {:else if costPlansQuery.isPending}
            <div class="flex flex-col gap-2 py-1" aria-busy="true">
              <Skeleton class="h-4 w-40" />
              <Skeleton class="h-4 w-56" />
            </div>
          {:else if costPlans.length === 0}
            <p class="py-1 text-sm text-muted-foreground">{m.cost_plan_no_history()}</p>
          {:else}
            <ul class="border-y" aria-label={m.cost_plan_history()}>
              {#each costPlans as plan (plan.id)}
                <li class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 py-2 not-first:border-t">
                  <span class="min-w-0 text-sm">{planSummary(plan)}</span>
                  <span class="font-technical text-xs text-muted-foreground tabular-nums"
                    >{formatLogTime(plan.effective_from)}</span>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      </FormSection>

      {#if formError || connectionNotice}
        <p
          class={formError ? 'text-sm text-destructive' : 'text-sm text-muted-foreground'}
          role={formError ? 'alert' : 'status'}>
          {formError || connectionNotice}
        </p>
      {/if}
      <!-- 固定操作区：在文档流中保留空间，不覆盖最后一个字段；取消在前、保存在后。 -->
      <div
        class="sticky bottom-0 z-20 flex translate-y-2 flex-wrap justify-end gap-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        {#if descriptor && channel}
          <Button
            type="button"
            variant="outline"
            class="me-auto"
            data-provider-connection-check
            onclick={() => void testConnection()}
            disabled={testing || busy}
            aria-busy={testing}
            aria-describedby="provider-connection-check-hint">
            {#if testing}<Spinner data-icon="inline-start" />{/if}
            <!-- 保留按钮宽度：检查中标签替换为「测试中…」，原标签只占位不朗读。 -->
            <span class="grid">
              <span class="col-start-1 row-start-1 {testing ? 'invisible' : ''}">{m.providers_test_connection()}</span>
              {#if testing}
                <span class="col-start-1 row-start-1">{m.providers_connection_check_testing()}</span>
              {/if}
            </span>
          </Button>
          <span id="provider-connection-check-hint" class="sr-only">{m.providers_connection_check_draft_hint()}</span>
        {/if}
        <Button
          type="button"
          variant="outline"
          onclick={() => guard.request(revert)}
          disabled={busy || (!dirty && !draft.oauthSessionId)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={busy || !canSave}>
          {#if busy}<Spinner data-icon="inline-start" />{/if}
          {m.provider_connection_view_save_connection()}
        </Button>
      </div>
    </form>
    <ProviderConnectionStatus {provider} oauth={oauthProvider} discovery={supportsDiscovery} />
  </div>
</section>

<DraftDiscardDialog {guard} />
