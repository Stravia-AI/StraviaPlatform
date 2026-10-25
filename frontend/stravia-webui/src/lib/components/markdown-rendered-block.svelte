<script lang="ts">
import type { MarkdownBlock, MarkdownImage } from '#lib/markdown.js'
import { createMarkdownBlockRenderer } from '#lib/markdown-dom.js'
import { buttonVariants } from '#lib/components/ui/button/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as m from '#lib/paraglide/messages.js'

let {
  block,
  streaming = false,
  clientBase = '',
}: { block: MarkdownBlock; streaming?: boolean; clientBase?: string } = $props()
let openedImage = $state<MarkdownImage | null>(null)
let imageFailed = $state(false)
let imageTrigger: HTMLButtonElement | null = null
function renderBlock(element: HTMLDivElement) {
  const renderer = createMarkdownBlockRenderer(element, (image, trigger) => {
    openedImage = image
    imageFailed = false
    imageTrigger = trigger
  })
  $effect(() => {
    renderer.update(
      block,
      streaming,
      {
        math: m.markdown_math_error(),
        mermaid: m.markdown_mermaid_error(),
        image: m.markdown_image_error(),
        openImage: m.markdown_image_open(),
        downloadImage: m.markdown_image_download(),
      },
      clientBase,
    )
  })
  return renderer.destroy
}
</script>

<div class="markdown-rendered-block" data-markdown-dialog-open={openedImage ? '' : undefined} {@attach renderBlock}>
</div>
{#if openedImage}
  {@const image = openedImage}
  <Dialog.Root
    open
    onOpenChange={(open) => {
      if (!open) openedImage = null
    }}>
    <Dialog.Layout
      class="sm:max-w-5xl"
      aria-describedby={undefined}
      onCloseAutoFocus={(event: Event) => {
        if (imageTrigger?.isConnected) {
          event.preventDefault()
          imageTrigger.focus()
        }
        imageTrigger = null
      }}>
      {#snippet header()}
        <Dialog.Title>{m.markdown_image_open()}</Dialog.Title>
      {/snippet}
      {#if imageFailed}
        <p class="text-sm text-muted-foreground">{m.markdown_image_error()}</p>
      {:else}
        <img
          src={image.url}
          alt={image.alt}
          referrerpolicy="no-referrer"
          class="max-h-[70dvh] max-w-full self-center object-contain"
          onerror={() => (imageFailed = true)} />
      {/if}
      {#if image.artifact}
        <code class="text-xs break-all text-muted-foreground">{image.artifact}</code>
      {/if}
      {#snippet footer()}
        {#if !imageFailed}
          <a
            class={buttonVariants({ variant: 'outline' })}
            href={image.url}
            target="_blank"
            rel="external noopener noreferrer"
            referrerpolicy="no-referrer"
            download="">
            {m.markdown_image_download()}
          </a>
        {/if}
      {/snippet}
    </Dialog.Layout>
  </Dialog.Root>
{/if}

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
.markdown-rendered-block :global(.markdown-artifact-image) {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 0.5rem;
  margin-block: 0.5rem;
  max-width: 100%;
}
.markdown-rendered-block :global(.markdown-artifact-image > img) {
  display: block;
  max-width: 100%;
  max-height: 36rem;
  object-fit: contain;
  border-radius: var(--radius-lg);
}
.markdown-rendered-block :global(.markdown-artifact-image > code) {
  overflow-wrap: anywhere;
  font-size: 0.85em;
}
.markdown-rendered-block :global(.markdown-image-actions) {
  display: flex;
  flex-wrap: wrap;
  gap: 1rem;
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
