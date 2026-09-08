<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount } from 'svelte'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatLogTime, formatNumber } from '$lib/format'
import { observationStatusLabel } from '$lib/observation-labels'
import type { CredentialDiscoverySummary, CredentialMatch, CredentialRule } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import * as Accordion from '$lib/components/ui/accordion'
import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import { Textarea } from '$lib/components/ui/textarea'

const settingKey = 'reversible_redaction_enabled'
const queryKey = ['setting', settingKey]
const queryClient = useQueryClient()
const settingQuery = createQuery(() => ({ queryKey, queryFn: () => admin.settings.get(settingKey) }))
const rulesQuery = createQuery(() => ({
  queryKey: ['credential-protection-rules'],
  queryFn: admin.credentialProtection.rules,
}))
let draft = $state<boolean>()
let saving = $state(false)
let saveError = $state('')
const storedEnabled = $derived(settingQuery.data === 'true')
const enabled = $derived(draft ?? storedEnabled)
const canSave = $derived(settingQuery.isSuccess && !saving && enabled !== storedEnabled)
let search = $state('')
let visibleRuleCount = $state(30)
const rules = $derived(rulesQuery.data?.rules ?? [])
const ruleNames = $derived(new Map(rules.map((rule) => [rule.id, rule.name])))
const filteredRules = $derived.by(() => {
  const term = search.trim().toLocaleLowerCase()
  return term
    ? rules.filter((rule) =>
        `${rule.name} ${rule.target} ${rule.id} ${rule.description}`.toLocaleLowerCase().includes(term),
      )
    : rules
})
const visibleRules = $derived(filteredRules.slice(0, visibleRuleCount))
let discoveries = $state.raw<CredentialDiscoverySummary[]>([])
let nextCursor = $state<string | null>(null)
let discoveriesLoading = $state(true)
let discoveriesError = $state(false)
let observationGap = $state(false)
let discoveryRetryAppend = false
let testInput = $state('')
let inputElement = $state<HTMLTextAreaElement | null>(null)
let testing = $state(false)
let testError = $state(false)
let matches = $state.raw<CredentialMatch[]>()

onMount(() => {
  void loadDiscoveries(false)
})

async function save(): Promise<void> {
  if (!canSave) return
  const value = enabled ? 'true' : 'false'
  saving = true
  saveError = ''
  try {
    await admin.settings.set(settingKey, value)
    queryClient.setQueryData(queryKey, value)
    draft = undefined
    toast.success(m.reversible_redaction_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}

async function loadDiscoveries(append: boolean): Promise<void> {
  discoveriesLoading = true
  discoveriesError = false
  discoveryRetryAppend = append
  try {
    const result = await admin.credentialProtection.discoveries({
      cursor: append ? (nextCursor ?? undefined) : undefined,
      limit: 20,
    })
    discoveries = append ? [...discoveries, ...result.items] : result.items
    nextCursor = result.next_cursor
    observationGap = append ? observationGap || result.observation_gap : result.observation_gap
  } catch {
    discoveriesError = true
  } finally {
    discoveriesLoading = false
  }
}

function conditions(rule: CredentialRule): string {
  const { path, secret_group, keywords, filter, components, skip_report, specificity, confidence } = rule
  return JSON.stringify(
    { path, secret_group, keywords, filter, components, skip_report, specificity, confidence },
    null,
    2,
  )
}

function sourceLabel(source: string): string {
  switch (source) {
    case 'user_message':
      return m.credential_protection_source_user_message()
    case 'system_or_history':
      return m.credential_protection_source_system_or_history()
    case 'tool_arguments':
      return m.credential_protection_source_tool_arguments()
    case 'tool_result':
      return m.credential_protection_source_tool_result()
    case 'other_text':
      return m.credential_protection_source_other_text()
    default:
      return m.credential_protection_source_unknown()
  }
}

async function testText(): Promise<void> {
  if (testing || !testInput.length) return
  testing = true
  testError = false
  matches = undefined
  try {
    matches = (await admin.credentialProtection.test(testInput)).matches
  } catch {
    testError = true
  } finally {
    testing = false
  }
}

function clearTest(): void {
  testInput = ''
  matches = undefined
  testError = false
  inputElement?.focus()
}

function selectMatch(match: CredentialMatch): void {
  inputElement?.focus()
  inputElement?.setSelectionRange(match.start, match.end)
  inputElement?.scrollIntoView({ block: 'center', behavior: 'instant' })
}
</script>

<svelte:head><title>{m.reversible_redaction_title()} · Stravia</title></svelte:head>

{#snippet pageActions()}
  <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
    {#if saving}<Spinner data-icon="inline-start" />{/if}
    {m.common_save_settings()}
  </Button>
{/snippet}

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.reversible_redaction_title()}
    description={m.reversible_redaction_summary()}
    actions={pageActions} />

  <section class="route-section flex flex-col gap-4" aria-labelledby="redaction-setting-title">
    <Field.Group>
      <Field.Field orientation="horizontal" data-disabled={!settingQuery.isSuccess || saving}>
        <Field.Content>
          <Field.Label id="redaction-setting-title" for="reversible-redaction-enabled"
            >{m.reversible_redaction_enable()}</Field.Label>
          <Field.Description id="redaction-setting-description">{m.reversible_redaction_scope()}</Field.Description>
        </Field.Content>
        <Switch
          id="reversible-redaction-enabled"
          checked={enabled}
          onCheckedChange={(checked) => {
            draft = checked
          }}
          disabled={!settingQuery.isSuccess || saving}
          aria-describedby="redaction-setting-description redaction-off-description" />
      </Field.Field>
    </Field.Group>
    {#if settingQuery.isPending}
      <p class="text-sm text-muted-foreground" role="status">{m.reversible_redaction_loading()}</p>
    {:else if settingQuery.isError}
      <Alert.Root variant="destructive"
        ><Alert.Description>{m.reversible_redaction_load_failed()}</Alert.Description></Alert.Root>
      <Button class="self-start" variant="outline" onclick={() => void settingQuery.refetch()}
        >{m.common_retry()}</Button>
    {:else}
      <p class="text-sm text-muted-foreground" role="status">
        {m.credential_protection_saved_state({
          state: storedEnabled ? m.credential_protection_on() : m.credential_protection_off(),
        })}{#if enabled !== storedEnabled}
          · {m.credential_protection_unsaved()}{/if}
      </p>
    {/if}
    {#if saveError}<Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root
      >{/if}
    <p id="redaction-off-description" class="text-sm text-muted-foreground">{m.credential_protection_off_brief()}</p>
    <Accordion.Root type="single">
      <Accordion.Item value="safety">
        <Accordion.Trigger>{m.credential_protection_details()}</Accordion.Trigger>
        <Accordion.Content>
          <div class="flex flex-col gap-3 text-sm text-muted-foreground">
            <p>{m.reversible_redaction_protection()}</p>
            <p>{m.reversible_redaction_mapping()}</p>
            <p>{m.reversible_redaction_failures()}</p>
            <p>{m.reversible_redaction_off()}</p>
            <p>{m.reversible_redaction_tools_warning()}</p>
            <p>{m.reversible_redaction_limits()}</p>
          </div>
        </Accordion.Content>
      </Accordion.Item>
    </Accordion.Root>
  </section>

  <section class="route-section flex min-w-0 flex-col gap-4" aria-labelledby="credential-rules-title">
    <div class="route-section-header">
      <h2 id="credential-rules-title" class="route-section-title">{m.credential_protection_catalog_title()}</h2>
    </div>
    <p class="text-sm text-muted-foreground">{m.credential_protection_catalog_intro()}</p>
    {#if rulesQuery.isPending}
      <p role="status">{m.credential_protection_catalog_loading()}</p>
    {:else if rulesQuery.isError}
      <Alert.Root variant="destructive"
        ><Alert.Description>{m.credential_protection_catalog_failed()}</Alert.Description></Alert.Root>
      <Button class="self-start" variant="outline" onclick={() => void rulesQuery.refetch()}>{m.common_retry()}</Button>
    {:else if rules.length === 0}
      <Empty.Root
        ><Empty.Header><Empty.Description>{m.credential_protection_catalog_empty()}</Empty.Description></Empty.Header
        ></Empty.Root>
    {:else}
      <Field.Group
        ><Field.Field>
          <Field.Label for="credential-rule-search">{m.credential_protection_catalog_search()}</Field.Label>
          <Input
            id="credential-rule-search"
            type="search"
            bind:value={search}
            oninput={() => {
              visibleRuleCount = 30
            }} />
        </Field.Field></Field.Group>
      <p class="text-sm text-muted-foreground" role="status">
        {m.credential_protection_catalog_count({
          shown: formatNumber(visibleRules.length),
          total: formatNumber(rules.length),
        })}
      </p>
      <Accordion.Root type="multiple">
        <Accordion.Item value="shared-conditions">
          <Accordion.Trigger>{m.credential_protection_catalog_conditions()}</Accordion.Trigger>
          <Accordion.Content>
            <p class="mb-3 text-sm text-muted-foreground">{m.credential_protection_catalog_path_note()}</p>
            <pre
              class="max-w-full overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 text-xs">{JSON.stringify(
                { prefilter: rulesQuery.data?.prefilter, filter: rulesQuery.data?.filter },
                null,
                2,
              )}</pre>
          </Accordion.Content>
        </Accordion.Item>
        {#each visibleRules as rule (rule.id)}
          <Accordion.Item value={rule.id}>
            <Accordion.Trigger
              ><span class="flex min-w-0 flex-col gap-1 text-left"
                ><span>{rule.name}</span><span class="text-xs font-normal text-muted-foreground"
                  >{m.credential_protection_rule_target()}: {rule.target}</span
                ><span class="text-sm font-normal text-muted-foreground">{rule.description}</span></span
              ></Accordion.Trigger>
            <Accordion.Content>
              <div class="flex min-w-0 flex-col gap-3">
                <p class="break-all font-mono text-xs">ID: {rule.id}</p>
                <h3 class="text-sm font-medium">{m.credential_protection_rule_expression()}</h3>
                <pre
                  class="max-w-full overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 text-xs">{rule.regex ??
                    m.credential_protection_rule_no_expression()}</pre>
                <h3 class="text-sm font-medium">{m.credential_protection_rule_conditions()}</h3>
                <p class="text-sm text-muted-foreground">{m.credential_protection_rule_conditions_help()}</p>
                <pre
                  class="max-w-full overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 text-xs">{conditions(
                    rule,
                  )}</pre>
              </div>
            </Accordion.Content>
          </Accordion.Item>
        {/each}
      </Accordion.Root>
      {#if filteredRules.length === 0}<Empty.Root
          ><Empty.Header
            ><Empty.Description>{m.credential_protection_catalog_no_results()}</Empty.Description></Empty.Header
          ></Empty.Root
        >{/if}
      {#if visibleRules.length < filteredRules.length}<Button
          class="self-start"
          variant="outline"
          onclick={() => {
            visibleRuleCount += 30
          }}>{m.credential_protection_catalog_more()}</Button
        >{/if}
    {/if}
  </section>

  <section class="route-section flex min-w-0 flex-col gap-4" aria-labelledby="credential-discoveries-title">
    <div class="route-section-header flex flex-wrap items-center justify-between gap-3">
      <h2 id="credential-discoveries-title" class="route-section-title">
        {m.credential_protection_discoveries_title()}
      </h2>
      <Button variant="outline" size="sm" disabled={discoveriesLoading} onclick={() => void loadDiscoveries(false)}
        >{m.credential_protection_discoveries_refresh()}</Button>
    </div>
    <p class="text-sm text-muted-foreground">{m.credential_protection_discoveries_intro()}</p>
    <p class="text-sm text-muted-foreground">{m.credential_protection_discoveries_retention()}</p>
    {#if observationGap}<Alert.Root
        ><Alert.Description>{m.credential_protection_discoveries_gap()}</Alert.Description></Alert.Root
      >{/if}
    {#if discoveriesError}
      <Alert.Root variant="destructive"
        ><Alert.Description>{m.credential_protection_discoveries_failed()}</Alert.Description></Alert.Root>
      <Button
        class="self-start"
        variant="outline"
        disabled={discoveriesLoading}
        onclick={() => void loadDiscoveries(discoveryRetryAppend)}>{m.common_retry()}</Button>
    {/if}
    {#if discoveriesLoading}<p role="status">{m.credential_protection_discoveries_loading()}</p>{/if}
    {#if !discoveriesLoading && !discoveriesError && !observationGap && discoveries.length === 0}
      <Empty.Root
        ><Empty.Header
          ><Empty.Description>{m.credential_protection_discoveries_empty()}</Empty.Description></Empty.Header
        ></Empty.Root>
    {/if}
    <Accordion.Root type="multiple">
      {#each discoveries as discovery (discovery.interaction_id)}
        <Accordion.Item value={discovery.interaction_id}>
          <Accordion.Trigger
            ><span class="flex min-w-0 flex-col gap-1 text-left"
              ><span
                >{m.credential_protection_discoveries_count({ count: formatNumber(discovery.new_credential_count) })} · {discovery.api_key_name ??
                  m.credential_protection_discoveries_unknown_key()}</span
              ><span class="text-xs font-normal text-muted-foreground">{formatLogTime(discovery.discovered_at)}</span
              ></span
            ></Accordion.Trigger>
          <Accordion.Content>
            <dl class="grid min-w-0 grid-cols-1 gap-2 text-sm sm:grid-cols-[auto_1fr] sm:gap-x-6">
              <dt class="text-muted-foreground">{m.credential_protection_discoveries_time()}</dt>
              <dd>{formatLogTime(discovery.discovered_at)}</dd>
              <dt class="text-muted-foreground">{m.credential_protection_discoveries_api_key()}</dt>
              <dd class="break-all">{discovery.api_key_name ?? m.credential_protection_discoveries_unknown_key()}</dd>
              <dt class="text-muted-foreground">{m.credential_protection_discoveries_rules()}</dt>
              <dd>
                <ul>
                  {#each discovery.rule_ids as id (id)}<li class="break-all">
                      {ruleNames.get(id) ?? id} <span class="text-xs text-muted-foreground">({id})</span>
                    </li>{/each}
                </ul>
              </dd>
              <dt class="text-muted-foreground">{m.credential_protection_discoveries_sources()}</dt>
              <dd>{discovery.source_types.map(sourceLabel).join(' · ')}</dd>
              <dt class="text-muted-foreground">{m.credential_protection_discoveries_status()}</dt>
              <dd>{observationStatusLabel(discovery.status)}</dd>
            </dl>
            {#if discovery.observation_gap}<Alert.Root class="mt-3"
                ><Alert.Description>{m.credential_protection_discoveries_gap()}</Alert.Description></Alert.Root
              >{/if}
            <Button
              class="mt-4"
              variant="outline"
              href={resolve(`/logs?interaction=${encodeURIComponent(discovery.interaction_id)}`)}
              >{m.credential_protection_discoveries_open()}</Button>
          </Accordion.Content>
        </Accordion.Item>
      {/each}
    </Accordion.Root>
    {#if nextCursor}<Button
        class="self-start"
        variant="outline"
        disabled={discoveriesLoading}
        onclick={() => void loadDiscoveries(true)}>{m.credential_protection_discoveries_more()}</Button
      >{/if}
  </section>

  <section class="route-section flex min-w-0 flex-col gap-4" aria-labelledby="credential-tester-title">
    <div class="route-section-header">
      <h2 id="credential-tester-title" class="route-section-title">{m.credential_protection_tester_title()}</h2>
    </div>
    <p class="text-sm text-muted-foreground">{m.credential_protection_tester_intro()}</p>
    <p id="credential-test-flow" class="text-sm text-muted-foreground">{m.credential_protection_tester_flow()}</p>
    <p id="credential-test-limits" class="text-sm text-muted-foreground">{m.credential_protection_tester_limits()}</p>
    <form
      onsubmit={(event) => {
        event.preventDefault()
        void testText()
      }}
      autocomplete="off">
      <Field.Group>
        <Field.Field data-disabled={testing}>
          <Field.Label for="credential-test-input">{m.credential_protection_tester_input()}</Field.Label>
          <Textarea
            id="credential-test-input"
            bind:ref={inputElement}
            bind:value={testInput}
            rows={7}
            class="min-h-40 max-h-96 resize-y"
            spellcheck={false}
            autocomplete="off"
            autocapitalize="off"
            disabled={testing}
            aria-describedby="credential-test-flow credential-test-limits"
            oninput={() => {
              matches = undefined
              testError = false
            }} />
        </Field.Field>
        <Field.Field orientation="horizontal" class="flex-wrap">
          <Button type="submit" disabled={testing || !testInput.length} aria-busy={testing}
            >{#if testing}<Spinner data-icon="inline-start" />{/if}{testing
              ? m.credential_protection_tester_running()
              : m.credential_protection_tester_submit()}</Button>
          <Button type="button" variant="outline" disabled={testing} onclick={clearTest}
            >{m.credential_protection_tester_clear()}</Button>
        </Field.Field>
      </Field.Group>
    </form>
    {#if testError}<Alert.Root variant="destructive"
        ><Alert.Description>{m.credential_protection_tester_failed()}</Alert.Description></Alert.Root
      >{/if}
    {#if matches}
      <div role="status" class="text-sm">
        {matches.length
          ? m.credential_protection_tester_matches({ count: formatNumber(matches.length) })
          : m.credential_protection_tester_none()}
      </div>
      <ul class="flex flex-col gap-2">
        {#each matches as match (`${match.rule_id}:${match.start}:${match.end}`)}
          <li>
            <Button
              variant="outline"
              class="h-auto w-full justify-start whitespace-normal py-3 text-left"
              onclick={() => selectMatch(match)}
              ><span class="flex min-w-0 flex-col gap-1"
                ><span class="break-all"
                  >{ruleNames.get(match.rule_id) ?? match.rule_id}
                  <span class="text-xs text-muted-foreground">({match.rule_id})</span></span
                ><span
                  >{m.credential_protection_tester_position({
                    startLine: match.start_line,
                    startColumn: match.start_column,
                    endLine: match.end_line,
                    endColumn: match.end_column,
                  })}</span
                ></span
              ></Button>
          </li>
        {/each}
      </ul>
    {/if}
  </section>
</div>
