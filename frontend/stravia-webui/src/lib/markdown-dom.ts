import DOMPurify from 'dompurify'
import { MARKDOWN_SANITIZE, renderMarkdownFragment, type MarkdownBlock, type MarkdownFragment } from './markdown'

interface RenderErrors {
  math: string
  mermaid: string
}

interface FragmentEntry {
  node: HTMLElement
  fragment: MarkdownFragment
  status: 'source' | 'pending' | 'rendered' | 'failed'
  error: HTMLElement | null
}

function identity(fragment: MarkdownFragment): string {
  return `${fragment.id}:${fragment.kind}:${fragment.display}:${fragment.complete}:${fragment.source}`
}

function fitDiagram(svg: SVGSVGElement): void {
  const content = svg.getBBox()
  const viewport = svg.viewBox.baseVal
  if (
    content.x >= viewport.x &&
    content.y >= viewport.y &&
    content.x + content.width <= viewport.x + viewport.width &&
    content.y + content.height <= viewport.y + viewport.height
  )
    return
  // Mermaid's temporary-DOM bounds can omit final node transforms. Fit the
  // sanitized, mounted geometry once rather than retaining a clipped viewport.
  const padding = 8
  const width = content.width + padding * 2
  const height = content.height + padding * 2
  svg.setAttribute('viewBox', `${content.x - padding} ${content.y - padding} ${width} ${height}`)
  svg.style.maxWidth = `${width}px`
}

/** Owns only the empty attachment host; Svelte never owns or reconciles its children. */
export function createMarkdownBlockRenderer(root: HTMLDivElement) {
  const entries = new Map<string, FragmentEntry>()
  let previousHtml: string | undefined
  let disposed = false
  let streaming = false
  let errors: RenderErrors = { math: '', mermaid: '' }

  function presentError(entry: FragmentEntry) {
    if (!streaming && (entry.status === 'failed' || !entry.fragment.complete)) {
      if (!entry.error) {
        entry.error = document.createElement('span')
        entry.error.className = 'markdown-render-error'
        entry.node.append(entry.error)
      }
      entry.error.textContent = errors[entry.fragment.kind]
    } else {
      entry.error?.remove()
      entry.error = null
    }
  }

  function render(entry: FragmentEntry, key: string) {
    if (!entry.fragment.complete || entry.status !== 'source') return
    entry.status = 'pending'
    void renderMarkdownFragment(entry.fragment)
      .then((html) => {
        if (disposed || entries.get(key) !== entry) return
        // This output was sanitized by the dedicated controlled renderer, not the ordinary Markdown sink.
        entry.node.innerHTML = html
        if (entry.fragment.kind === 'mermaid') {
          const svg = entry.node.querySelector<SVGSVGElement>('svg')
          if (svg) fitDiagram(svg)
        }
        entry.error = null
        entry.status = 'rendered'
      })
      .catch(() => {
        if (disposed || entries.get(key) !== entry) return
        entry.status = 'failed'
        presentError(entry)
      })
  }

  function update(block: MarkdownBlock, live: boolean, messages: RenderErrors) {
    if (disposed) return
    streaming = live
    errors = messages
    const keys = new Set(block.fragments.map(identity))
    for (const key of entries.keys()) if (!keys.has(key)) entries.delete(key)

    if (previousHtml !== block.html) {
      const template = document.createElement('template')
      template.innerHTML = DOMPurify.sanitize(block.html, MARKDOWN_SANITIZE)
      for (const fragment of block.fragments) {
        const placeholder = template.content.querySelector<HTMLElement>(`[data-markdown-fragment="${fragment.id}"]`)
        if (!placeholder) continue
        const key = identity(fragment)
        const existing = entries.get(key)
        if (existing) {
          existing.fragment = fragment
          // Move, rather than recreate, completed and pending nodes while surrounding text appends.
          placeholder.replaceWith(existing.node)
          if (existing.status !== 'rendered') {
            const source = existing.node.querySelector('code')
            if (source) source.textContent = fragment.raw
          }
        } else {
          placeholder.classList.toggle('markdown-fragment-block', fragment.display)
          entries.set(key, { node: placeholder, fragment, status: 'source', error: null })
        }
      }
      root.replaceChildren(template.content)
      previousHtml = block.html
    }

    for (const fragment of block.fragments) {
      const key = identity(fragment)
      const entry = entries.get(key)
      if (!entry) continue
      presentError(entry)
      render(entry, key)
    }
  }

  function destroy() {
    disposed = true
    entries.clear()
  }

  return { update, destroy }
}
