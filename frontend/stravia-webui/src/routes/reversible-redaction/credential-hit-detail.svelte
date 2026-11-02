<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { CredentialHitDetail, CredentialHitField } from '#lib/types.js'

import { entrySegments, segmentOwner } from '#lib/credential-hit-records.js'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import FileWarningIcon from '@lucide/svelte/icons/file-warning'

interface Props {
  detail: CredentialHitDetail
  ruleNames: ReadonlyMap<string, string>
  /** 当前是否显示服务端返回的明文；遮罩结果仍留在缓存里。 */
  plaintext: boolean
  revealPending: boolean
  revealFailure: 'forbidden' | 'failed' | null
  onreveal: () => void
  onhide: () => void
}

let { detail, ruleNames, plaintext, revealPending, revealFailure, onreveal, onhide }: Props = $props()

let entryElement = $state<HTMLElement | null>(null)

const hitMatchIndex = $derived(detail.state === 'available' ? detail.matches.findIndex((match) => match.hit) : -1)
const segments = $derived(detail.state === 'available' ? entrySegments(detail.text, detail.matches) : [])
// 唯一的匹配就是命中本身时，它已出现在命中行与高亮里，列表不再提供新信息。
const showMatches = $derived(
  detail.state === 'available' && detail.matches.length > 0 && !(detail.matches.length === 1 && hitMatchIndex === 0),
)
const canReveal = $derived(plaintext || (detail.state === 'available' && detail.matches.length > 0))

// 列表里最后点选的一行；绑定到当时的 detail，切换命中、显示或隐藏明文后响应换了，选择随之失效。
// raw：detail 是缓存里的普通对象，放进 $state 会被代理而无法按引用比较。
let selection = $state.raw<{ detail: CredentialHitDetail; index: number } | null>(null)
const selectedIndex = $derived(selection?.detail === detail ? selection.index : null)
// 被合并进同一令牌或与前一条重叠的匹配没有自己的片段，高亮落在实际渲染它的那一块上。
const activeSegment = $derived(
  detail.state === 'available' && selectedIndex !== null ? segmentOwner(segments, detail.matches, selectedIndex) : null,
)

// 只滚动条目框自身：scrollIntoView 会连带滚动外层侧栏，把标题与命中行顶出视野。
function scrollToMatch(index: number): HTMLElement | null {
  const box = entryElement
  if (!box || detail.state !== 'available') return null
  const owner = segmentOwner(segments, detail.matches, index)
  if (owner === null) return null
  const target = box.querySelector<HTMLElement>(`[data-match-index="${owner}"]`)
  if (!target) return null
  box.scrollTop = target.offsetTop - (box.clientHeight - target.offsetHeight) / 2
  return target
}

// 条目可能很长；打开时直接停在命中处，替代单独的上下文摘录。
// 遮罩与明文切换后正文长度变化，也重新把命中放回视野中央。
$effect(() => {
  if (hitMatchIndex >= 0) scrollToMatch(hitMatchIndex)
})

// 点选是用户操作，条目框若被侧栏滚出了视野，要连侧栏一起带到能看见高亮的位置；
// nearest 在已可见时不滚动，不会像打开时那样顶掉标题。
function selectMatch(index: number): void {
  if (detail.state !== 'available') return
  selection = { detail, index }
  const target = scrollToMatch(index)
  if (!target) return
  target.scrollIntoView({ block: 'nearest', inline: 'nearest' })
  // 持续的描边只标出「是哪一块」；再点同一行时它没有变化，用一次短脉冲确认点击生效。
  if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
    target.animate(
      [
        { outlineOffset: '0px', outlineColor: 'var(--primary)' },
        { outlineOffset: '5px', outlineColor: 'transparent' },
      ],
      { duration: 480, easing: 'cubic-bezier(0.2, 0, 0, 1)' },
    )
  }
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
    {#if canReveal}
      <div class="flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          aria-pressed={plaintext}
          disabled={revealPending}
          onclick={plaintext ? onhide : onreveal}>
          {#if revealPending}
            <Spinner data-icon="inline-start" aria-label={m.common_loading()} />
          {:else if plaintext}
            <EyeOffIcon data-icon="inline-start" />
          {:else}
            <EyeIcon data-icon="inline-start" />
          {/if}
          {plaintext ? m.credential_protection_hit_records_hide() : m.credential_protection_hit_records_reveal()}
        </Button>
      </div>
    {/if}
    {#if revealFailure === 'forbidden'}
      <Alert.Root variant="destructive">
        <Alert.Description>{m.credential_protection_hit_records_reveal_forbidden()}</Alert.Description>
      </Alert.Root>
    {:else if revealFailure === 'failed'}
      <Alert.Root variant="destructive">
        <Alert.Description>{m.credential_protection_hit_records_detail_failed()}</Alert.Description>
      </Alert.Root>
      <Button class="self-start" variant="outline" size="sm" onclick={onreveal}>{m.common_retry()}</Button>
    {/if}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      bind:this={entryElement}
      class="entry-box"
      data-slot="credential-entry"
      data-masked={detail.masked}
      tabindex="0"
      aria-label={m.credential_protection_hit_records_full_entry()}>
      {#each segments as segment, index (index)}
        {#if segment.kind === 'plain'}
          <span>{segment.text}</span>
        {:else}
          <span
            class={segment.kind === 'hit' ? 'entry-hit' : 'entry-match'}
            data-match-index={segment.matchIndex}
            data-active={segment.matchIndex === activeSegment ? 'true' : undefined}>{segment.text}</span>
        {/if}
      {/each}
    </div>

    {#if showMatches}
      <h4 class="mt-1 flex items-baseline gap-2 text-sm font-semibold">
        {m.credential_protection_hit_records_matches()}
        <span class="text-xs font-normal text-muted-foreground">{detail.matches.length}</span>
      </h4>
      <ul class="match-box">
        {#each detail.matches as match, index (`${match.rule_id}:${match.start}:${match.end}`)}
          {@const name = ruleNames.get(match.rule_id) ?? match.rule_id}
          {@const position = m.credential_protection_tester_position({
            startLine: match.start_line,
            startColumn: match.start_column,
            endLine: match.end_line,
            endColumn: match.end_column,
          })}
          <li>
            <button
              type="button"
              class="match-row"
              aria-current={selectedIndex === index ? 'true' : undefined}
              title={`${name} (${match.rule_id}) · ${position}`}
              onclick={() => selectMatch(index)}>
              <span class="match-name">{name} <span class="match-rule">({match.rule_id})</span></span>
              <!-- 行列简写只给眼睛看；完整的位置说明留给读屏与悬停提示。 -->
              <span class="match-position" aria-hidden="true"
                >{match.start_line}:{match.start_column}–{match.end_line}:{match.end_column}</span>
              <span class="sr-only">{position}</span>
            </button>
          </li>
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
.entry-box .entry-hit {
  border-radius: 2px;
  background: color-mix(in oklab, var(--primary) 18%, transparent);
  box-shadow: inset 0 -1px 0 var(--primary);
}
.entry-box .entry-match {
  border-radius: 2px;
  background: color-mix(in oklab, var(--warning) 18%, transparent);
  box-shadow: inset 0 -1px 0 var(--warning);
  color: var(--muted-foreground);
}
/* 遮罩模式下服务端只发回固定令牌：命中与其他匹配都保持虚线芯片外观，不可逐条点击。 */
.entry-box[data-masked='true'] .entry-hit,
.entry-box[data-masked='true'] .entry-match {
  border-radius: 4px;
  border: 1px dashed var(--muted-foreground);
  background: var(--muted);
  box-shadow: none;
  padding: 0 0.35em;
  color: var(--muted-foreground);
}
/* 点选列表某一行后，正文里对应的那一块用实线描边加深底色标出；遮罩时所有令牌外观相同，靠它区分。 */
.entry-box .entry-hit[data-active='true'],
.entry-box .entry-match[data-active='true'] {
  outline: 2px solid var(--primary);
  outline-offset: 1px;
  background: color-mix(in oklab, var(--primary) 30%, transparent);
  box-shadow: none;
  color: var(--foreground);
}
.entry-box[data-masked='true'] .entry-hit[data-active='true'],
.entry-box[data-masked='true'] .entry-match[data-active='true'] {
  border-style: solid;
  border-color: var(--primary);
  background: color-mix(in oklab, var(--primary) 30%, var(--muted));
  color: var(--foreground);
}
/* 匹配逐行紧凑排列：规则名在左、行列范围在右，与条目框同一套边界；
   行数多时在框内滚动，不撑高侧栏。 */
.match-box {
  max-height: 12rem;
  overflow: auto;
  border: 1px solid var(--input);
  border-radius: var(--radius-lg);
  background: var(--background);
}
.match-box > li + li {
  border-top: 1px solid var(--border);
}
.match-row {
  display: flex;
  width: 100%;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.75rem;
  padding: 0.3125rem 0.625rem;
  text-align: start;
  font-size: 0.8125rem;
  line-height: 1.25rem;
  color: var(--muted-foreground);
  transition: background-color 140ms cubic-bezier(0.2, 0, 0, 1);
}
.match-row:hover {
  background: var(--accent);
}
.match-row:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: -2px;
}
/* 当前点选的一行：底色与正文高亮同色系，前景色加深。 */
.match-row[aria-current='true'] {
  background: color-mix(in oklab, var(--primary) 14%, transparent);
  color: var(--foreground);
}
.match-name {
  min-width: 0;
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.match-rule {
  font-size: 0.75rem;
  color: var(--muted-foreground);
}
.match-position {
  flex-shrink: 0;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  color: var(--muted-foreground);
}
</style>
