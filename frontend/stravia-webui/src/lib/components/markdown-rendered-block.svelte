<script lang="ts">
import type { MarkdownBlock } from '$lib/markdown'
import { createMarkdownBlockRenderer } from '$lib/markdown-dom'
import * as m from '$lib/paraglide/messages.js'

let { block, streaming = false }: { block: MarkdownBlock; streaming?: boolean } = $props()
function renderBlock(element: HTMLDivElement) {
  const renderer = createMarkdownBlockRenderer(element)
  $effect(() => {
    renderer.update(block, streaming, { math: m.markdown_math_error(), mermaid: m.markdown_mermaid_error() })
  })
  return renderer.destroy
}
</script>

<div class="markdown-rendered-block" {@attach renderBlock}></div>

<style>
.markdown-rendered-block {
  display: contents;
}
.markdown-rendered-block :global(.markdown-fragment-block) {
  display: block;
  max-width: 100%;
  overflow-x: auto;
  margin-block: 0.35rem;
}
.markdown-rendered-block :global(.markdown-fragment-block > code) {
  display: block;
  white-space: pre-wrap;
}
.markdown-rendered-block :global(.markdown-render-error) {
  display: block;
  font-family: var(--font-body);
  font-size: 0.85em;
  color: var(--muted-foreground);
  margin-block-start: 0.25rem;
}
.markdown-rendered-block :global(.markdown-fragment-block > svg) {
  display: block;
  max-width: 100%;
  height: auto;
  margin-inline: auto;
}
.markdown-rendered-block :global(.markdown-fragment-block > svg text),
.markdown-rendered-block :global(.markdown-fragment-block > svg tspan) {
  fill: var(--foreground) !important;
}
.markdown-rendered-block :global(.markdown-fragment-block > svg :is(rect, circle, ellipse, polygon)) {
  fill: var(--muted) !important;
  stroke: var(--border) !important;
}
.markdown-rendered-block
  :global(
    .markdown-fragment-block > svg :is(line, polyline, .edgePath path, .flowchart-link, .messageLine0, .messageLine1)
  ) {
  stroke: var(--muted-foreground) !important;
}
.markdown-rendered-block :global(.markdown-fragment-block > svg marker path) {
  fill: var(--muted-foreground) !important;
  stroke: var(--muted-foreground) !important;
}
.markdown-rendered-block :global(.markdown-fragment-block > svg .cluster rect),
.markdown-rendered-block :global(.markdown-fragment-block > svg .labelBkg) {
  fill: var(--background) !important;
}
.markdown-rendered-block :global(.katex-display) {
  margin-block: 0.35rem;
}
@media (prefers-reduced-motion: reduce) {
  .markdown-rendered-block :global(.markdown-fragment-block > svg *) {
    animation: none !important;
    transition: none !important;
  }
}
</style>
