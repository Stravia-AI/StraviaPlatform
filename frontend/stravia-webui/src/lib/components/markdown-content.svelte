<script lang="ts">
import DOMPurify from 'dompurify'
import { marked } from 'marked'

let { text }: { text: string } = $props()

// 用原文起点区分重复段落，追加正文时不替换已经完成的块。
const blocks = $derived.by(() => {
  let offset = 0
  return marked.lexer(text, { gfm: true, breaks: true }).map((token) => {
    const id = offset
    offset += token.raw.length
    return {
      id,
      html: DOMPurify.sanitize(marked.parser([token], { async: false, gfm: true, breaks: true }), {
        ALLOWED_TAGS: [
          'p',
          'br',
          'strong',
          'em',
          'del',
          'code',
          'pre',
          'blockquote',
          'ul',
          'ol',
          'li',
          'h1',
          'h2',
          'h3',
          'h4',
          'h5',
          'h6',
          'hr',
          'table',
          'thead',
          'tbody',
          'tr',
          'th',
          'td',
        ],
        ALLOWED_ATTR: ['start'],
        ALLOW_DATA_ATTR: false,
        ALLOW_ARIA_ATTR: false,
      }),
    }
  })
})
</script>

<div class="markdown-content">
  {#each blocks as block (block.id)}
    <!-- HTML 仅包含安全过滤后的非交互 Markdown 排版。 -->
    <!-- eslint-disable-next-line svelte/no-at-html-tags -->
    {@html block.html}
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
.markdown-content :global(:first-child) {
  margin-top: 0;
}
.markdown-content :global(:last-child) {
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
.markdown-content > :global(:is(p, pre, blockquote, ul, ol, table):first-child) {
  margin-top: var(--markdown-first-margin, 0);
}
</style>
