import DOMPurify from 'dompurify'
import { buttonVariants } from '#lib/components/ui/button/index.js'
import {
  MARKDOWN_SANITIZE,
  renderMarkdownFragment,
  type MarkdownBlock,
  type MarkdownFragment,
  type MarkdownImage,
} from './markdown'

const IMAGE_ACTION_CLASS = buttonVariants({ variant: 'link', size: 'sm' })

interface RenderErrors {
  math: string
  mermaid: string
  image: string
  openImage: string
  downloadImage: string
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
export function createMarkdownBlockRenderer(
  root: HTMLDivElement,
  onOpenImage: (image: MarkdownImage, trigger: HTMLButtonElement) => void,
) {
  const entries = new Map<string, FragmentEntry>()
  const images = new Map<string, ReturnType<typeof renderImage>>()
  let previousHtml: string | undefined
  let previousBase: string | undefined
  let disposed = false
  let streaming = false
  let errors: RenderErrors = { math: '', mermaid: '', image: '', openImage: '', downloadImage: '' }

  function imageUrl(value: string, clientBase: string): string | null {
    if (!clientBase) return null
    try {
      const base = new URL(clientBase)
      const url = new URL(value)
      if (
        !['http:', 'https:'].includes(base.protocol) ||
        base.username ||
        base.password ||
        base.search ||
        base.hash ||
        url.origin !== base.origin ||
        url.username ||
        url.password ||
        url.search ||
        url.hash
      )
        return null
      const prefix = `${base.pathname.replace(/\/+$/, '')}/v1/artifacts/downloads/`
      return url.pathname.startsWith(prefix) && /^[0-9a-f]{64}$/.test(url.pathname.slice(prefix.length))
        ? url.href
        : null
    } catch {
      return null
    }
  }

  function renderImage(image: MarkdownImage, clientBase: string) {
    const host = document.createElement('span')
    host.className = 'markdown-artifact-image'
    const unavailable = document.createElement('span')
    unavailable.className = 'markdown-render-error'
    unavailable.textContent = errors.image
    if (image.alt.trim()) {
      const caption = document.createElement('span')
      caption.textContent = image.alt
      host.append(caption)
    }
    const url = imageUrl(image.url, clientBase)
    if (!url) {
      host.append(unavailable)
      return {
        node: host,
        updateMessages(messages: RenderErrors) {
          unavailable.textContent = messages.image
        },
      }
    }
    const preview = document.createElement('img')
    preview.alt = image.alt
    preview.referrerPolicy = 'no-referrer'
    const actions = document.createElement('span')
    actions.className = 'markdown-image-actions'
    const open = document.createElement('button')
    open.type = 'button'
    open.className = IMAGE_ACTION_CLASS
    open.textContent = errors.openImage
    open.setAttribute('aria-haspopup', 'dialog')
    open.addEventListener('click', () => onOpenImage({ ...image, url }, open))
    const download = document.createElement('a')
    download.className = IMAGE_ACTION_CLASS
    download.href = url
    download.textContent = errors.downloadImage
    download.target = '_blank'
    download.rel = 'external noopener noreferrer'
    download.referrerPolicy = 'no-referrer'
    download.download = ''
    actions.append(open, download)
    preview.addEventListener(
      'error',
      () => {
        preview.remove()
        actions.remove()
        host.append(unavailable)
      },
      { once: true },
    )
    host.prepend(preview)
    host.append(actions)
    preview.src = url
    return {
      node: host,
      updateMessages(messages: RenderErrors) {
        unavailable.textContent = messages.image
        open.textContent = messages.openImage
        download.textContent = messages.downloadImage
      },
    }
  }

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

  function update(block: MarkdownBlock, live: boolean, messages: RenderErrors, clientBase = '') {
    if (disposed) return
    streaming = live
    errors = messages
    const keys = new Set(block.fragments.map(identity))
    for (const key of entries.keys()) if (!keys.has(key)) entries.delete(key)
    const imageKey = (image: MarkdownImage) => JSON.stringify([image.id, image.url, image.alt])
    const imageKeys = new Set(block.images.map(imageKey))
    const imagesChanged = imageKeys.size !== images.size || [...imageKeys].some((key) => !images.has(key))
    for (const key of images.keys()) if (!imageKeys.has(key)) images.delete(key)
    if (previousBase !== clientBase) images.clear()

    if (previousHtml !== block.html || previousBase !== clientBase || imagesChanged) {
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
      for (const image of block.images) {
        const placeholder = template.content.querySelector<HTMLElement>(`[data-markdown-image="${image.id}"]`)
        if (!placeholder) continue
        const key = imageKey(image)
        let node = images.get(key)
        if (!node) {
          node = renderImage(image, clientBase)
          images.set(key, node)
        }
        placeholder.replaceWith(node.node)
      }
      root.replaceChildren(template.content)
      previousHtml = block.html
      previousBase = clientBase
    }

    for (const image of images.values()) image.updateMessages(messages)
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
    images.clear()
  }

  return { update, destroy }
}
