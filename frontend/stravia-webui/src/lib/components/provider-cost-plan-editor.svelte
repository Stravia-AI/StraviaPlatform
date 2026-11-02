<script lang="ts" module>
import type { ProviderCostDraftFields } from '#lib/provider-cost-draft.js'
import type { CustomCurrency, NewProviderCostPlan, ProviderCostPlan } from '#lib/types.js'

export interface ProviderCostPlanEditorProps {
  open: boolean
  /** 当前生效的成本方式；与草稿相同时没有可保存的变更。 */
  plan: ProviderCostPlan | null | undefined
  /** 采用建议时带入的草稿；为空时从当前生效记录起步。 */
  prefill?: ProviderCostDraftFields | null
  customCurrency: CustomCurrency | null
  /** 追加一条生效记录；失败 throw，Sheet 保留输入并显示原因。 */
  onsave: (input: NewProviderCostPlan) => Promise<void>
}
</script>

<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { currencyForKind } from '#lib/money.js'
import { costDraftFromPlan, costDraftIssue, costDraftMatchesPlan, costPlanInput } from '#lib/provider-cost-draft.js'
import type { CostBasis, CurrencyKind, SubscriptionPeriod } from '#lib/types.js'

let { open = $bindable(false), plan, prefill = null, customCurrency, onsave }: ProviderCostPlanEditorProps = $props()

const idPrefix = $props.id()
const basisLabelId = `${idPrefix}-basis`
const periodLabelId = `${idPrefix}-period`

let saving = $state(false)
let fields = $state<ProviderCostDraftFields>(untrack(() => costDraftFromPlan(plan)))
let attempted = $state(false)
let error = $state('')
let amountInput = $state<HTMLInputElement | null>(null)
let focusAmountOnOpen = false
let content = $state<HTMLElement | null>(null)

// 草稿离开保护：打开时的草稿为基线；保存中锁定提交、编辑与关闭。
const baseline = new DraftBaseline(() => fields)
const guard = new DraftGuard({ dirty: () => open && baseline.dirty, busy: () => saving })

const issue = $derived(costDraftIssue(fields))
const showIssue = $derived(Boolean(issue) && (fields.amount.length > 0 || attempted))
// 与当前生效记录相同则无可保存内容；从建议带入的草稿即使未被编辑也与当前记录不同，可直接保存。
const unchanged = $derived(costDraftMatchesPlan(fields, plan))
const basisHelp = $derived(
  fields.basis === 'subscription'
    ? m.cost_basis_subscription_help()
    : fields.basis === 'none'
      ? m.cost_basis_none_help()
      : m.cost_basis_usage_help(),
)
const symbol = $derived(currencyForKind(fields.currency, customCurrency).symbol)

let wasOpen = false
$effect.pre(() => {
  const isOpen = open
  if (isOpen && !wasOpen) untrack(loadDraft)
  wasOpen = isOpen
})

function loadDraft(): void {
  fields = prefill ? { ...prefill } : costDraftFromPlan(plan)
  attempted = false
  error = ''
  baseline.reset()
  // 建议只识别出订阅而没有标价时，光标直接交给金额；此时 Sheet 内容尚未挂载，由 onOpenAutoFocus 接手。
  focusAmountOnOpen = fields.basis === 'subscription' && fields.amount === ''
}

function setBasis(value: string): void {
  if (!value || value === fields.basis) return
  const previous = fields.basis
  fields.basis = value as CostBasis
  // 新建订阅价格时沿用展示币种；已有价格写法保持不变。
  if (value === 'subscription' && previous !== 'subscription' && !fields.amount) {
    fields.currency = customCurrency ? 'custom' : 'usd'
  }
  attempted = false
  error = ''
}

function setPeriod(value: string): void {
  if (value && value !== fields.period) fields.period = value as SubscriptionPeriod
}

function setCurrency(value: string): void {
  if (value && value !== fields.currency) fields.currency = value as CurrencyKind
}

async function submit(): Promise<void> {
  if (saving || unchanged) return
  const input = costPlanInput(fields)
  if (!input) {
    attempted = true
    amountInput?.focus()
    return
  }
  saving = true
  error = ''
  try {
    await onsave(input)
    baseline.reset()
    open = false
    toast.success(m.cost_plan_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<Sheet.Root bind:open={() => open, guard.openChange((next) => (open = next))}>
  <Sheet.Content
    bind:ref={content}
    side="right"
    class="route-overlay-content w-full! gap-0 overflow-hidden p-0 sm:max-w-[32rem]!"
    closeLabel={m.common_close()}
    locked={guard.busy}
    onOpenAutoFocus={(event: Event) => {
      // 默认会聚焦第一个可聚焦元素（标题旁的「?」），使说明气泡一打开就弹出。
      event.preventDefault()
      const selected = content?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]')
      ;(focusAmountOnOpen ? amountInput : selected)?.focus()
    }}>
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title>{m.cost_plan_edit_title()}</Sheet.Title>
    </Sheet.Header>
    <form
      class="route-overlay-form"
      onsubmit={(event) => {
        event.preventDefault()
        void submit()
      }}>
      <div class="route-overlay-body flex flex-col gap-4">
        <Field.Group>
          <Field.Field>
            <Field.Label id={basisLabelId} hint={m.cost_plan_effective_help()}>{m.cost_plan_basis()}</Field.Label>
            <Tabs.Root bind:value={() => fields.basis, setBasis}>
              <Tabs.List class="w-full" aria-labelledby={basisLabelId}>
                <Tabs.Trigger class="flex-1" value="usage" disabled={saving}>{m.cost_basis_usage()}</Tabs.Trigger>
                <Tabs.Trigger class="flex-1" value="subscription" disabled={saving}
                  >{m.cost_basis_subscription()}</Tabs.Trigger>
                <Tabs.Trigger class="flex-1" value="none" disabled={saving}>{m.cost_basis_none()}</Tabs.Trigger>
              </Tabs.List>
            </Tabs.Root>
            <Field.Description>{basisHelp}</Field.Description>
          </Field.Field>

          {#if fields.basis === 'subscription'}
            <Field.Field>
              <Field.Label id={periodLabelId}>{m.cost_plan_period()}</Field.Label>
              <Tabs.Root bind:value={() => fields.period, setPeriod}>
                <Tabs.List class="w-full" aria-labelledby={periodLabelId}>
                  <Tabs.Trigger class="flex-1" value="month" disabled={saving}>{m.cost_period_month()}</Tabs.Trigger>
                  <Tabs.Trigger class="flex-1" value="year" disabled={saving}>{m.cost_period_year()}</Tabs.Trigger>
                </Tabs.List>
              </Tabs.Root>
            </Field.Field>

            <Field.Field data-invalid={showIssue || undefined}>
              <Field.Label for="provider-cost-amount">{m.cost_plan_price()}</Field.Label>
              <InputGroup.Root>
                <InputGroup.Addon align="inline-start">
                  <InputGroup.Text class="font-technical">{symbol}</InputGroup.Text>
                </InputGroup.Addon>
                <InputGroup.Input
                  id="provider-cost-amount"
                  bind:ref={amountInput}
                  bind:value={fields.amount}
                  inputmode="decimal"
                  autocomplete="off"
                  class="font-technical"
                  placeholder="0.00"
                  disabled={saving}
                  aria-invalid={showIssue || undefined}
                  oninput={() => (attempted = false)} />
                <InputGroup.Addon align="inline-end">
                  <InputGroup.Text
                    >{fields.period === 'year' ? m.cost_plan_per_year() : m.cost_plan_per_month()}</InputGroup.Text>
                </InputGroup.Addon>
              </InputGroup.Root>
              {#if showIssue}<Field.Error>{issue}</Field.Error>{:else}<Field.Description
                  >{m.cost_plan_price_help()}</Field.Description
                >{/if}
            </Field.Field>

            {#if customCurrency}
              <Field.Field>
                <Field.Label for="provider-cost-currency">{m.cost_plan_currency()}</Field.Label>
                <Select.Root type="single" value={fields.currency} onValueChange={setCurrency} disabled={saving}>
                  <Select.Trigger id="provider-cost-currency" class="w-full" aria-label={m.cost_plan_currency()}>
                    {fields.currency === 'custom' ? customCurrency.symbol : m.pricing_currency_usd()}
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
          {/if}
        </Field.Group>
        {#if error}
          <Alert.Root variant="destructive" role="alert">
            <Alert.Description>{error}</Alert.Description>
          </Alert.Root>
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        <Button type="button" variant="outline" disabled={saving} onclick={() => guard.request(() => (open = false))}
          >{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving || unchanged} aria-busy={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save()}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

<DraftDiscardDialog {guard} />
