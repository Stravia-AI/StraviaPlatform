<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { resolve } from '$app/paths'
import type { CredentialDiscoveryRecord } from '#lib/types.js'

import {
  credentialSourceLabel,
  displayRuleSets,
  hitQueryFailure,
  hitSourceKind,
  requestLogPath,
  ruleSetLabel,
  uniqueReuses,
} from '#lib/credential-hit-records.js'
import { credentialHitDetailQuery, credentialHitReusesQuery } from '#lib/credential-protection-queries.js'
import { formatLogTime } from '#lib/format.js'

import CredentialHitDetail from './credential-hit-detail.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'

interface Props {
  open?: boolean
  record: CredentialDiscoveryRecord | null
  hitIndex?: number
  ruleNames: ReadonlyMap<string, string>
}

let { open = $bindable(false), record, hitIndex = $bindable(0), ruleNames }: Props = $props()

const selectedHit = $derived(record?.hits[hitIndex] ?? null)
const detailQuery = createQuery(() =>
  credentialHitDetailQuery(record?.id ?? null, hitIndex, open && selectedHit !== null),
)
const reusesQuery = createQuery(() => credentialHitReusesQuery(record?.id ?? null, open && record?.linkable === true))
const reuses = $derived(reusesQuery.data ? uniqueReuses(reusesQuery.data.items) : [])

const title = $derived(
  record
    ? m.credential_protection_hit_records_detail_title({
        apiKey: record.api_key_name ?? m.credential_protection_discoveries_unknown_key(),
        time: formatLogTime(record.created_at),
      })
    : m.credential_protection_discoveries_title(),
)
</script>

<Sheet.Root bind:open>
  <Sheet.Content
    closeLabel={m.common_close()}
    class="data-[side=right]:w-full data-[side=right]:sm:max-w-xl overflow-y-auto">
    <Sheet.Header class="pr-12">
      <Sheet.Title>{title}</Sheet.Title>
    </Sheet.Header>
    {#if record}
      <div class="flex min-w-0 flex-col gap-5 px-4 pb-6">
        <dl class="summary-meta">
          <dt>{m.credential_protection_hit_records_rule_sets()}</dt>
          <dd>
            {#if record.rule_sets.length}
              <span class="flex flex-wrap gap-1.5">
                {#each displayRuleSets(record.rule_sets) as ruleSet (ruleSet)}
                  <Badge variant={ruleSet === 'builtin' ? 'outline' : 'secondary'}>{ruleSetLabel(ruleSet)}</Badge>
                {/each}
              </span>
            {:else}
              <span class="text-muted-foreground">—</span>
            {/if}
          </dd>
          <dt>{m.credential_protection_hit_records_created_by()}</dt>
          <dd>
            {#if record.rule_ids.length}
              <span class="flex flex-wrap gap-1.5">
                {#each record.rule_ids as ruleId (ruleId)}
                  <Badge variant="outline" class="h-auto max-w-full whitespace-normal" title={ruleId}
                    >{ruleNames.get(ruleId) ?? ruleId}</Badge>
                {/each}
              </span>
            {:else}
              <span class="text-muted-foreground">—</span>
            {/if}
          </dd>
        </dl>

        <section aria-labelledby="hit-list-title">
          <h3 id="hit-list-title" class="mb-2 text-sm font-medium">
            {m.credential_protection_hit_records_hits()}
          </h3>
          {#if record.hits.length}
            <ul class="flex flex-col gap-1.5">
              {#each record.hits as hit, index (`${hit.rule_id}:${hitSourceKind(hit)}:${index}`)}
                <li>
                  <Button
                    variant={index === hitIndex ? 'secondary' : 'ghost'}
                    class="h-auto w-full justify-start gap-1.5 whitespace-normal py-2 text-left"
                    aria-pressed={index === hitIndex}
                    onclick={() => (hitIndex = index)}>
                    <span class="font-medium">{ruleNames.get(hit.rule_id) ?? hit.rule_id}</span>
                    <span aria-hidden="true" class="text-muted-foreground">←</span>
                    <span class="sr-only">{m.credential_protection_hits_found_in()}</span>
                    <span class="text-muted-foreground">{credentialSourceLabel(hitSourceKind(hit))}</span>
                  </Button>
                </li>
              {/each}
            </ul>
          {:else}
            <p class="entry-note">{m.credential_protection_hit_records_no_hits()}</p>
          {/if}
        </section>

        {#if selectedHit}
          {#key `${record.id}:${hitIndex}`}
            {#if detailQuery.isPending}
              <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
                ><Spinner />{m.credential_protection_hit_records_detail_loading()}</span>
            {:else if detailQuery.isError}
              {#if hitQueryFailure(detailQuery.error) === 'gone'}
                <p class="entry-note">{m.credential_protection_hit_records_detail_gone()}</p>
              {:else}
                <Alert.Root variant="destructive"
                  ><Alert.Description>{m.credential_protection_hit_records_detail_failed()}</Alert.Description
                  ></Alert.Root>
                <Button class="self-start" variant="outline" onclick={() => void detailQuery.refetch()}
                  >{m.common_retry()}</Button>
              {/if}
            {:else if detailQuery.data}
              <CredentialHitDetail detail={detailQuery.data} {ruleNames} />
            {/if}
          {/key}
        {/if}

        <section aria-labelledby="hit-reuse-title">
          <h3 id="hit-reuse-title" class="mb-2 text-sm font-medium">
            {m.credential_protection_hit_records_reuse()}
          </h3>
          {#if !record.linkable}
            <p class="entry-note">{m.credential_protection_hit_records_reuse_unlinkable()}</p>
          {:else if reusesQuery.isPending}
            <span class="inline-flex items-center gap-2 text-sm text-muted-foreground" role="status"
              ><Spinner />{m.credential_protection_hit_records_reuse_loading()}</span>
          {:else if reusesQuery.isError}
            {#if hitQueryFailure(reusesQuery.error) === 'gone'}
              <p class="entry-note">{m.credential_protection_hit_records_detail_gone()}</p>
            {:else}
              <Alert.Root variant="destructive"
                ><Alert.Description>{m.credential_protection_hit_records_reuse_failed()}</Alert.Description
                ></Alert.Root>
              <Button class="mt-2 self-start" variant="outline" onclick={() => void reusesQuery.refetch()}
                >{m.common_retry()}</Button>
            {/if}
          {:else if reusesQuery.data}
            {#if !reusesQuery.data.linkable}
              <p class="entry-note">{m.credential_protection_hit_records_reuse_unlinkable()}</p>
            {:else if !reuses.length}
              <p class="entry-note">{m.credential_protection_hit_records_reuse_empty()}</p>
            {:else}
              <ul class="reuse-list">
                {#each reuses as item (`${item.interaction_id}:${item.source_type}:${item.tool_id ?? item.latest_input ?? ''}`)}
                  <li>
                    <a class="reuse-link" href={resolve(requestLogPath(item.interaction_id))}>
                      <span class="flex min-w-0 flex-col gap-0.5">
                        <span class="font-medium">{credentialSourceLabel(hitSourceKind(item))}</span>
                        <span class="text-xs text-muted-foreground"
                          >{item.api_key_name ?? m.credential_protection_discoveries_unknown_key()}</span>
                        <span class="font-technical text-xs text-muted-foreground"
                          >{m.credential_protection_hit_records_reuse_last({
                            time: formatLogTime(item.last_reused_at),
                          })}</span>
                      </span>
                      <span class="sr-only">{m.credential_protection_hit_records_open_request()}</span>
                      <ArrowRightIcon class="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                    </a>
                  </li>
                {/each}
              </ul>
              {#if reusesQuery.data.truncated}
                <p class="mt-2 text-xs text-muted-foreground">
                  {m.credential_protection_hit_records_reuse_truncated()}
                </p>
              {/if}
            {/if}
          {/if}
        </section>
      </div>
    {/if}
  </Sheet.Content>
</Sheet.Root>

<style>
.summary-meta {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  align-items: baseline;
  gap: 0.5rem 1rem;
  font-size: 0.8125rem;
}
.summary-meta dt {
  color: var(--muted-foreground);
}
.entry-note {
  padding: 0.75rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--muted);
  font-size: 0.8125rem;
  color: var(--muted-foreground);
}
.reuse-list {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.reuse-list > li + li {
  border-top: 1px solid var(--border);
}
.reuse-link {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.75rem;
  padding: 0.625rem 0.75rem;
  transition: background-color 140ms cubic-bezier(0.2, 0, 0, 1);
}
.reuse-link:hover {
  background: var(--accent);
}
.reuse-link:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: -2px;
}
</style>
