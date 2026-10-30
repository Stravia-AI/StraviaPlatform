<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { tick } from 'svelte'
import { SvelteSet } from 'svelte/reactivity'
import type { CredentialHitDetail, CredentialHitField } from '#lib/types.js'

import {
  credentialSourceLabel,
  entryContext,
  entrySegments,
  HIT_MASK,
  maskedEntryText,
} from '#lib/credential-hit-records.js'
import CredentialMatchItem from '#lib/components/credential-match-item.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import FileWarningIcon from '@lucide/svelte/icons/file-warning'

interface Props {
  detail: CredentialHitDetail
  ruleNames: ReadonlyMap<string, string>
}

let { detail, ruleNames }: Props = $props()

// 命中片段默认遮罩；组件在切换命中时由外层 key 重新创建，因此状态自然回到遮罩。
const revealed = new SvelteSet<number>()
let entryElement = $state<HTMLElement | null>(null)

const hitMatchIndex = $derived(detail.state === 'available' ? detail.matches.findIndex((match) => match.hit) : -1)
const segments = $derived(detail.state === 'available' ? entrySegments(detail.text, detail.matches, revealed) : [])
// 上下文行与完整条目共用遮罩状态，展开一处即两处同步。
const context = $derived(
  detail.state === 'available' && hitMatchIndex >= 0
    ? entryContext(maskedEntryText(segments), detail.matches[hitMatchIndex]?.start_line ?? 1)
    : null,
)

function toggleMatch(index: number): void {
  if (revealed.has(index)) revealed.delete(index)
  else revealed.add(index)
}

async function selectMatch(index: number): Promise<void> {
  if (!revealed.has(index)) {
    revealed.add(index)
    await tick()
  }
  entryElement
    ?.querySelector(`[data-match-index="${index}"]`)
    ?.scrollIntoView({ block: 'nearest', behavior: 'instant' })
}
</script>

{#snippet fieldMeta(field: CredentialHitField)}
  <dl class="field-meta">
    <dt>{m.credential_protection_hit_records_source()}</dt>
    <dd>{credentialSourceLabel(field.source_type)}</dd>
    {#if field.tool_name}
      <dt>{m.credential_protection_hit_records_tool_name()}</dt>
      <dd>{field.tool_name}</dd>
    {/if}
    {#if field.tool_id}
      <dt>{m.credential_protection_hit_records_tool_id()}</dt>
      <dd><TechnicalValue value={field.tool_id} copyable /></dd>
    {/if}
  </dl>
{/snippet}

{#if detail.state === 'unavailable'}
  <div class="flex min-w-0 flex-col gap-5">
    <section aria-labelledby="hit-field-context-title">
      <h3 id="hit-field-context-title" class="mb-2 text-sm font-medium">
        {m.credential_protection_hit_records_field_context()}
      </h3>
      {@render fieldMeta(detail.field)}
    </section>
    <section aria-labelledby="hit-entry-title">
      <h3 id="hit-entry-title" class="mb-2 text-sm font-medium">
        {m.credential_protection_hit_records_full_entry()}
      </h3>
      <Empty.Root class="border">
        <Empty.Header>
          <Empty.Media variant="icon"><FileWarningIcon /></Empty.Media>
          <Empty.Title>{m.credential_protection_hit_records_unavailable()}</Empty.Title>
          <Empty.Description
            >{detail.reason === 'not_retained'
              ? m.credential_protection_hit_records_unavailable_not_retained()
              : m.credential_protection_hit_records_unavailable_missing()}</Empty.Description>
        </Empty.Header>
      </Empty.Root>
    </section>
  </div>
{:else}
  <div class="flex min-w-0 flex-col gap-5">
    <section aria-labelledby="hit-field-context-title">
      <h3 id="hit-field-context-title" class="mb-2 text-sm font-medium">
        {m.credential_protection_hit_records_field_context()}
      </h3>
      {@render fieldMeta(detail.field)}
      {#if context}
        <div class="context-box">
          {#if context.hasBefore}<div class="context-ellipsis">…</div>{/if}
          {#each context.lines as line (line.number)}
            <div class="context-line">
              <span class="context-line-number">{line.number}</span><span class="context-line-text">{line.text}</span>
            </div>
          {/each}
          {#if context.hasAfter}<div class="context-ellipsis">…</div>{/if}
        </div>
      {:else if detail.matches.length === 0}
        <p class="entry-note">{m.credential_protection_hit_records_no_matches()}</p>
      {:else}
        <p class="entry-note">{m.credential_protection_hit_records_no_hit_match()}</p>
      {/if}
    </section>

    <section aria-labelledby="hit-entry-title">
      <h3 id="hit-entry-title" class="mb-2 text-sm font-medium">
        {m.credential_protection_hit_records_full_entry()}
      </h3>
      {#if detail.hit_basis === 'rule'}
        <Alert.Root class="mb-3">
          <Alert.Description>{m.credential_protection_hit_records_hit_basis_rule()}</Alert.Description>
        </Alert.Root>
      {/if}
      {#if detail.truncated}
        <p class="mb-2 text-xs text-muted-foreground">{m.credential_protection_hit_records_truncated()}</p>
      {/if}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div
        bind:this={entryElement}
        class="entry-box"
        data-slot="credential-entry"
        tabindex="0"
        aria-label={m.credential_protection_hit_records_full_entry()}>
        {#each segments as segment, index (index)}
          {#if segment.kind === 'plain'}
            <span>{segment.text}</span>
          {:else if segment.kind === 'hit' && !segment.revealed}
            <button
              type="button"
              class="entry-masked"
              data-match-index={segment.matchIndex}
              aria-label={m.credential_protection_hit_records_reveal()}
              aria-pressed="false"
              onclick={() => segment.matchIndex !== null && toggleMatch(segment.matchIndex)}>{HIT_MASK}</button>
          {:else if segment.kind === 'hit'}
            <button
              type="button"
              class="entry-hit"
              data-match-index={segment.matchIndex}
              aria-label={m.credential_protection_hit_records_hide()}
              aria-pressed="true"
              onclick={() => segment.matchIndex !== null && toggleMatch(segment.matchIndex)}>{segment.text}</button>
          {:else}
            <span class="entry-match" data-match-index={segment.matchIndex}>{segment.text}</span>
          {/if}
        {/each}
      </div>

      <h4 class="mt-4 mb-2 text-sm font-medium">{m.credential_protection_hit_records_matches()}</h4>
      {#if detail.matches.length}
        <ul class="flex flex-col gap-2">
          {#each detail.matches as match, index (`${match.rule_id}:${match.start}:${match.end}`)}
            <CredentialMatchItem
              {match}
              {ruleNames}
              emphasis={match.hit ? 'hit' : 'match'}
              onselect={() => void selectMatch(index)} />
          {/each}
        </ul>
      {/if}
    </section>
  </div>
{/if}

<style>
.field-meta {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  align-items: baseline;
  gap: 0.25rem 1rem;
  margin-bottom: 0.75rem;
  font-size: 0.8125rem;
}
.field-meta dt {
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
.context-box {
  display: flex;
  flex-direction: column;
  max-height: 14rem;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--muted);
  padding: 0.75rem;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  line-height: 1.7;
}
.context-line {
  display: flex;
  gap: 0.75rem;
}
.context-line-text {
  min-width: 0;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.context-line-number {
  flex: none;
  min-width: 2rem;
  color: var(--muted-foreground);
  text-align: end;
  user-select: none;
}
.context-ellipsis {
  color: var(--muted-foreground);
}
/* 与密钥匹配测试的输入框同一套边界与字体；正文可滚动，不撑高侧栏。 */
.entry-box {
  max-height: 24rem;
  overflow: auto;
  border: 1px solid var(--input);
  border-radius: var(--radius-lg);
  background: var(--background);
  padding: 0.5rem 0.625rem;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  line-height: 1.7;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.entry-box:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: 2px;
}
.entry-box button {
  border: 0;
  padding: 0;
  background: none;
  font: inherit;
  color: inherit;
}
.entry-box .entry-hit {
  border-radius: 2px;
  background: color-mix(in oklab, var(--primary) 18%, transparent);
  box-shadow: inset 0 -1px 0 var(--primary);
  cursor: pointer;
}
.entry-match {
  border-radius: 2px;
  background: color-mix(in oklab, var(--warning) 18%, transparent);
  box-shadow: inset 0 -1px 0 var(--warning);
  color: var(--muted-foreground);
}
.entry-box .entry-masked {
  border-radius: 4px;
  border: 1px dashed var(--muted-foreground);
  background: var(--muted);
  padding: 0 0.35em;
  color: var(--muted-foreground);
  cursor: pointer;
}
.entry-box .entry-hit,
.entry-box .entry-masked {
  font: inherit;
}
.entry-masked:focus-visible,
.entry-hit:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: 2px;
}
</style>
