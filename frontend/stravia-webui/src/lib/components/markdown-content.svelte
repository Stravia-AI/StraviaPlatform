<script lang="ts">
import { markdownBlocks } from '$lib/markdown'
import MarkdownRenderedBlock from '$lib/components/markdown-rendered-block.svelte'

let {
  text,
  minimumHeadingLevel = 1,
  streaming = false,
}: { text: string; minimumHeadingLevel?: 1 | 2; streaming?: boolean } = $props()

// 用原文起点区分重复段落，追加正文时不替换已经完成的块。
const blocks = $derived(markdownBlocks(text, minimumHeadingLevel))
</script>

<div class="markdown-content">
  {#each blocks as block (block.id)}
    <MarkdownRenderedBlock {block} {streaming} />
  {/each}
</div>

<style>
.markdown-content {
  flex: none;
  min-width: 0;
  overflow-wrap: anywhere;
}
.markdown-content :global(p),
.markdown-content :global(pre),
.markdown-content :global(blockquote),
.markdown-content :global(ul),
.markdown-content :global(ol),
.markdown-content :global(table) {
  margin-block: 0.35rem;
}
.markdown-content :global(.markdown-rendered-block:first-child > :first-child) {
  margin-top: 0;
}
.markdown-content :global(.markdown-rendered-block:last-child > :last-child) {
  margin-bottom: 0;
}
.markdown-content :global(:is(h1, h2, h3, h4, h5, h6)) {
  margin-block: 0.35rem;
  font-size: inherit;
  font-weight: 600;
}
.markdown-content :global(ul),
.markdown-content :global(ol) {
  padding-inline-start: 1.2rem;
}
.markdown-content :global(ul) {
  list-style: disc;
}
.markdown-content :global(ol) {
  list-style: decimal;
}
.markdown-content :global(code) {
  font-family: var(--font-technical);
  background: color-mix(in oklab, currentColor 12%, transparent);
}
.markdown-content :global(pre) {
  white-space: pre-wrap;
}
.markdown-content :global(blockquote) {
  border-inline-start: 2px solid var(--markdown-border, color-mix(in oklab, currentColor 25%, transparent));
  padding-inline-start: 0.5rem;
}
.markdown-content :global(table) {
  width: 100%;
  table-layout: fixed;
  border-collapse: collapse;
}
.markdown-content :global(th),
.markdown-content :global(td) {
  border: 1px solid var(--markdown-border, color-mix(in oklab, currentColor 25%, transparent));
  padding: 0.15rem;
}
.markdown-content :global(.markdown-rendered-block:first-child > :is(p, pre, blockquote, ul, ol, table):first-child) {
  margin-top: var(--markdown-first-margin, 0);
}
</style>
