<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { tick } from 'svelte'
import { SvelteSet } from 'svelte/reactivity'
import type { CredentialHitDetail, CredentialHitField } from '#lib/types.js'

import { entrySegments, HIT_MASK } from '#lib/credential-hit-records.js'
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
// 唯一的匹配就是命中本身时，它已出现在命中行与高亮里，列表不再提供新信息。
const showMatches = $derived(
  detail.state === 'available' && detail.matches.length > 0 && !(detail.matches.length === 1 && hitMatchIndex === 0),
)

// 只滚动条目框自身：scrollIntoView 会连带滚动外层侧栏，把标题与命中行顶出视野。
function scrollToMatch(index: number): void {
  const box = entryElement
  const target = box?.querySelector<HTMLElement>(`[data-match-index="${index}"]`)
  if (!box || !target) return
  box.scrollTop = target.offsetTop - (box.clientHeight - target.offsetHeight) / 2
}

// 条目可能很长；打开时直接停在命中处，替代单独的上下文摘录。
$effect(() => {
  if (hitMatchIndex >= 0) scrollToMatch(hitMatchIndex)
})

function toggleMatch(index: number): void {
  if (revealed.has(index)) revealed.delete(index)
  else revealed.add(index)
}

async function selectMatch(index: number): Promise<void> {
  if (!revealed.has(index)) {
    revealed.add(index)
    await tick()
  }
  scrollToMatch(index)
}
</script>

<!-- 来源已在命中行里，这里只补充命中行没有的工具信息。 -->
{#snippet fieldMeta(field: CredentialHitField)}
  {#if field.tool_name || field.tool_id}
    <dl class="field-meta">
      {#if field.tool_name}
        <dt>{m.credential_protection_hit_records_tool_name()}</dt>
        <dd>{field.tool_name}</dd>
      {/if}
      {#if field.tool_id}
        <dt>{m.credential_protection_hit_records_tool_id()}</dt>
        <dd><TechnicalValue value={field.tool_id} copyable /></dd>
      {/if}
    </dl>
  {/if}
{/snippet}

{#if detail.state === 'unavailable'}
  <div class="flex min-w-0 flex-col gap-3">
    {@render fieldMeta(detail.field)}
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
  </div>
{:else}
  <div class="flex min-w-0 flex-col gap-3">
    {@render fieldMeta(detail.field)}
    {#if detail.hit_basis === 'rule'}
      <Alert.Root>
        <Alert.Description>{m.credential_protection_hit_records_hit_basis_rule()}</Alert.Description>
      </Alert.Root>
    {/if}
    {#if hitMatchIndex < 0}
      <p class="text-sm text-muted-foreground">
        {detail.matches.length === 0
          ? m.credential_protection_hit_records_no_matches()
          : m.credential_protection_hit_records_no_hit_match()}
      </p>
    {/if}
    {#if detail.truncated}
      <p class="text-xs text-muted-foreground">{m.credential_protection_hit_records_truncated()}</p>
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
        {:else if segment.kind === 'hit'}
          <!-- 遮罩与明文共用同一个按钮：切换后焦点不丢，侧栏的 Escape 仍直接关闭侧栏。 -->
          <button
            type="button"
            class={segment.revealed ? 'entry-hit' : 'entry-masked'}
            data-match-index={segment.matchIndex}
            aria-label={segment.revealed
              ? m.credential_protection_hit_records_hide()
              : m.credential_protection_hit_records_reveal()}
            aria-pressed={segment.revealed}
            onclick={() => segment.matchIndex !== null && toggleMatch(segment.matchIndex)}
            >{segment.revealed ? segment.text : HIT_MASK}</button>
        {:else}
          <span class="entry-match" data-match-index={segment.matchIndex}>{segment.text}</span>
        {/if}
      {/each}
    </div>

    {#if showMatches}
      <h4 class="mt-2 text-sm font-semibold">{m.credential_protection_hit_records_matches()}</h4>
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
  </div>
{/if}

<style>
.field-meta {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  align-items: baseline;
  gap: 0.25rem 1rem;
  font-size: 0.8125rem;
}
.field-meta dt {
  color: var(--muted-foreground);
}
/* 与密钥匹配测试的输入框同一套边界与字体；正文可滚动，不撑高侧栏。
   position 让 offsetTop 相对条目框，打开时据此把命中滚到框中央。 */
.entry-box {
  position: relative;
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
