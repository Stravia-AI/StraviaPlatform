<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin, currencyQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatLogTime } from '#lib/format.js'
import { currencyForKind, formatCostPlanPrice } from '#lib/money.js'
import type {
  CostBasis,
  CurrencyKind,
  NewProviderCostPlan,
  Provider,
  ProviderCostPlan,
  SubscriptionPeriod,
} from '#lib/types.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Field from '#lib/components/ui/field/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

interface Props {
  provider: Provider
}

let { provider }: Props = $props()
const queryClient = useQueryClient()
const plansQuery = createQuery(() => ({
  queryKey: ['provider-cost-plans', provider.id],
  queryFn: () => admin.providers.costPlans(provider.id),
}))
const currencyQuery = createQuery(() => currencyQueryOptions())
const custom = $derived(currencyQuery.data ?? null)

let editorOpen = $state(false)
let saving = $state(false)
let error = $state('')
let basis = $state<CostBasis>('usage')
let period = $state<SubscriptionPeriod>('month')
let amount = $state('')
let priceCurrency = $state<CurrencyKind>('usd')
const decimalPattern = /^(?:0|[1-9]\d*)(?:\.\d+)?$/
const amountIssue = $derived.by(() => {
  if (basis === 'usage') return ''
  const value = amount.trim()
  return decimalPattern.test(value) && Number(value) >= 0 ? '' : m.cost_amount_invalid()
})
const canSave = $derived(basis === 'usage' || !amountIssue)

const plans = $derived(plansQuery.data ?? [])
const current = $derived(plans[0] ?? provider.cost_plan ?? null)
const summary = $derived(planSummary(current))

function planSummary(plan: ProviderCostPlan | null): string {
  if (!plan || plan.basis === 'usage') return m.cost_basis_usage()
  const periodLabel = plan.period === 'year' ? m.cost_period_year() : m.cost_period_month()
  return `${periodLabel} · ${formatCostPlanPrice(plan.price, custom)}`
}

function openEditor(): void {
  const plan = current
  basis = plan?.basis ?? 'usage'
  period = plan?.period ?? 'month'
  amount = plan?.price?.amount ?? ''
  priceCurrency = plan?.price?.currency ?? (custom ? 'custom' : 'usd')
  error = ''
  editorOpen = true
}

async function save(): Promise<void> {
  if (!canSave || saving) return
  const input: NewProviderCostPlan =
    basis === 'usage'
      ? { basis: 'usage' }
      : { basis: 'subscription', period, price: { amount: amount.trim(), currency: priceCurrency } }
  saving = true
  error = ''
  try {
    await admin.providers.appendCostPlan(provider.id, input)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['provider-cost-plans', provider.id] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-providers'] }),
    ])
    editorOpen = false
    toast.success(m.cost_plan_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<section class="route-section" aria-labelledby="provider-cost-plan-title">
  <div class="route-section-header">
    <div class="flex min-w-0 items-center gap-2">
      <h2 id="provider-cost-plan-title" class="route-section-title">{m.cost_plan_section()}</h2>
      <Field.Help hint={m.cost_plan_help()} />
    </div>
  </div>
  {#if plansQuery.error && plansQuery.data === undefined}
    <RequestFailure
      title={m.cost_plan_not_loaded()}
      message={localizeBackendErrorMessage(plansQuery.error)}
      retry={() => void plansQuery.refetch()}
      retrying={plansQuery.isFetching} />
  {:else if plansQuery.isPending}
    <div class="flex flex-col gap-3 py-3" aria-busy="true"><Skeleton class="h-9" /><Skeleton class="h-24" /></div>
  {:else}
    <SettingRow
      title={m.cost_plan_current()}
      description={m.cost_plan_effective_help()}
      value={summary}
      onedit={openEditor} />
    <div class="mt-4">
      <h3 class="mb-1 text-sm font-medium">{m.cost_plan_history()}</h3>
      {#if plans.length === 0}
        <p class="py-2 text-sm text-muted-foreground">{m.cost_plan_no_history()}</p>
      {:else}
        <ul class="border-y" aria-label={m.cost_plan_history()}>
          {#each plans as plan (plan.id)}
            <li class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 py-2.5 not-first:border-t">
              <span class="min-w-0 text-sm">{planSummary(plan)}</span>
              <span class="font-technical text-xs text-muted-foreground tabular-nums"
                >{formatLogTime(plan.effective_from)}</span>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</section>

<SettingsSheet
  bind:open={editorOpen}
  title={m.cost_plan_edit()}
  draft={() => ({ basis, period, amount, priceCurrency })}
  submitLabel={m.cost_plan_save()}
  submitDisabled={!canSave}
  {saving}
  {error}
  footerNote={m.cost_plan_effect_help()}
  onsubmit={save}
  onclose={() => (error = '')}>
  <Field.Field size="setting">
    <Field.FieldLabel for="provider-cost-basis">{m.cost_plan_basis()}</Field.FieldLabel>
    <Select.Root type="single" value={basis} onValueChange={(value: string) => value && (basis = value as CostBasis)}>
      <Select.Trigger id="provider-cost-basis" class="w-full" aria-label={m.cost_plan_basis()}>
        {basis === 'subscription' ? m.cost_basis_subscription() : m.cost_basis_usage()}
      </Select.Trigger>
      <Select.Content>
        <Select.Group>
          <Select.Item value="usage">{m.cost_basis_usage()}</Select.Item>
          <Select.Item value="subscription">{m.cost_basis_subscription()}</Select.Item>
        </Select.Group>
      </Select.Content>
    </Select.Root>
  </Field.Field>
  {#if basis === 'subscription'}
    <Field.Field size="setting">
      <Field.FieldLabel for="provider-cost-period">{m.cost_plan_period()}</Field.FieldLabel>
      <Select.Root
        type="single"
        value={period}
        onValueChange={(value: string) => value && (period = value as SubscriptionPeriod)}>
        <Select.Trigger id="provider-cost-period" class="w-full" aria-label={m.cost_plan_period()}>
          {period === 'year' ? m.cost_period_year() : m.cost_period_month()}
        </Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="month">{m.cost_period_month()}</Select.Item>
            <Select.Item value="year">{m.cost_period_year()}</Select.Item>
          </Select.Group>
        </Select.Content>
      </Select.Root>
    </Field.Field>
    <Field.Field data-invalid={amountIssue ? true : undefined}>
      <Field.FieldLabel for="provider-cost-amount">{m.cost_plan_price()}</Field.FieldLabel>
      <InputGroup.Root>
        <InputGroup.Addon align="inline-start">
          <InputGroup.Text class="font-technical">{currencyForKind(priceCurrency, custom).symbol}</InputGroup.Text>
        </InputGroup.Addon>
        <InputGroup.Input
          id="provider-cost-amount"
          bind:value={amount}
          inputmode="decimal"
          class="font-technical"
          placeholder="0.00"
          autocomplete="off" />
      </InputGroup.Root>
      <Field.FieldDescription>{m.cost_plan_price_help()}</Field.FieldDescription>
      {#if amountIssue && amount.length > 0}<Field.FieldError>{amountIssue}</Field.FieldError>{/if}
    </Field.Field>
    {#if custom}
      <Field.Field size="setting">
        <Field.FieldLabel for="provider-cost-currency">{m.cost_plan_currency()}</Field.FieldLabel>
        <Select.Root
          type="single"
          value={priceCurrency}
          onValueChange={(value: string) => value && (priceCurrency = value as CurrencyKind)}>
          <Select.Trigger id="provider-cost-currency" class="w-full" aria-label={m.cost_plan_currency()}>
            {priceCurrency === 'custom' ? custom.symbol : m.pricing_currency_usd()}
          </Select.Trigger>
          <Select.Content>
            <Select.Group>
              <Select.Item value="usd">{m.pricing_currency_usd()}</Select.Item>
              <Select.Item value="custom">{custom.symbol}</Select.Item>
            </Select.Group>
          </Select.Content>
        </Select.Root>
      </Field.Field>
    {/if}
  {/if}
</SettingsSheet>
