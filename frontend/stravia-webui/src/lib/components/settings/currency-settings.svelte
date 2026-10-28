<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin, currencyQueryKey, currencyQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatAmountNumber } from '#lib/money.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const queryClient = useQueryClient()
const currencyQuery = createQuery(() => currencyQueryOptions())
const currency = $derived(currencyQuery.data ?? null)

let editorOpen = $state(false)
let symbolDraft = $state('')
let rateDraft = $state('')
let saving = $state(false)
let error = $state('')
let confirmOpen = $state(false)
const ratePattern = /^(?:0|[1-9]\d*)(?:\.\d+)?$/

const symbolIssue = $derived.by(() => {
  const length = symbolDraft.trim().length
  return length < 1 || length > 8 ? m.currency_symbol_invalid() : ''
})
const rateIssue = $derived.by(() => {
  const value = rateDraft.trim()
  return !ratePattern.test(value) || Number(value) <= 0 ? m.currency_rate_invalid() : ''
})
const canSave = $derived(!symbolIssue && !rateIssue)

function openEditor(): void {
  symbolDraft = currency?.symbol ?? ''
  rateDraft = currency?.usd_rate ?? ''
  error = ''
  editorOpen = true
}

function requestSave(): void {
  if (!canSave) return
  if (!currency) {
    confirmOpen = true
    return
  }
  void save()
}

async function save(): Promise<void> {
  saving = true
  error = ''
  try {
    const saved = await admin.currency.save({ symbol: symbolDraft.trim(), usd_rate: rateDraft.trim() })
    queryClient.setQueryData(currencyQueryKey, saved)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-overview'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-series'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-providers'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-models'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-api-keys'] }),
    ])
    confirmOpen = false
    editorOpen = false
    toast.success(m.currency_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<SettingsSection id="currency" title={m.currency_custom_currency()}>
  {#if currencyQuery.error}
    <RequestFailure
      title={m.currency_not_loaded()}
      message={localizeBackendErrorMessage(currencyQuery.error)}
      retry={() => void currencyQuery.refetch()}
      retrying={currencyQuery.isFetching} />
  {:else if currencyQuery.isPending}
    <div class="flex flex-col gap-3 py-3" aria-busy="true"><Skeleton class="h-9" /></div>
  {:else}
    <SettingRow
      title={m.currency_display_currency()}
      description={currency ? undefined : m.currency_usd_default_help()}
      value={currency
        ? m.currency_summary({ symbol: currency.symbol, rate: formatAmountNumber(currency.usd_rate) })
        : m.currency_not_configured()}
      onedit={openEditor} />
  {/if}
</SettingsSection>

<SettingsSheet
  bind:open={editorOpen}
  title={currency ? m.currency_edit() : m.currency_set()}
  draft={() => ({ symbol: symbolDraft, rate: rateDraft })}
  submitLabel={m.currency_save()}
  submitDisabled={!canSave}
  {saving}
  {error}
  footerNote={currency ? m.currency_rate_change_help() : undefined}
  onsubmit={requestSave}
  onclose={() => (error = '')}>
  <Field.Field data-invalid={symbolIssue ? true : undefined}>
    <Field.FieldLabel for="currency-symbol">{m.currency_symbol()}</Field.FieldLabel>
    <Input
      id="currency-symbol"
      bind:value={symbolDraft}
      maxlength={8}
      disabled={saving}
      placeholder="¥"
      autocomplete="off" />
    <Field.FieldDescription>{m.currency_symbol_help()}</Field.FieldDescription>
    {#if symbolIssue && symbolDraft.length > 0}<Field.FieldError>{symbolIssue}</Field.FieldError>{/if}
  </Field.Field>
  <Field.Field data-invalid={rateIssue ? true : undefined}>
    <Field.FieldLabel for="currency-rate">{m.currency_rate()}</Field.FieldLabel>
    <Input
      id="currency-rate"
      bind:value={rateDraft}
      inputmode="decimal"
      disabled={saving}
      class="font-technical"
      placeholder="7.2"
      autocomplete="off" />
    <Field.FieldDescription>{m.currency_rate_help()}</Field.FieldDescription>
    {#if rateIssue && rateDraft.length > 0}<Field.FieldError>{rateIssue}</Field.FieldError>{/if}
  </Field.Field>
</SettingsSheet>

<AlertDialog.Root bind:open={confirmOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.currency_first_save_title()}</AlertDialog.Title>
      <AlertDialog.Description>{m.currency_first_save_body()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (confirmOpen = false)}>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action disabled={saving} onclick={() => void save()}
        >{m.currency_first_save_confirm()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
