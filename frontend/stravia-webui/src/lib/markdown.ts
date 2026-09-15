import { marked, Renderer } from 'marked'

const MARKED_OPTIONS = { async: false, gfm: true, breaks: true } as const

const HTML_ESCAPE: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
}

export const MARKDOWN_SANITIZE = {
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
}

export interface MarkdownBlock {
  id: number
  html: string
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (ch) => HTML_ESCAPE[ch] ?? ch)
}

// 原始 HTML 不能当 HTML 插入：`<title>` / `<textarea>` 会被浏览器抽走或隐藏，对话里只剩空气泡。
class ObservationRenderer extends Renderer {
  html({ text, block }: { text: string; block?: boolean }): string {
    const escaped = escapeHtml(text)
    return block ? `<p>${escaped}</p>` : escaped
  }
}

const renderer = new ObservationRenderer()

export function markdownBlocks(text: string): MarkdownBlock[] {
  let offset = 0
  return marked.lexer(text, MARKED_OPTIONS).map((token) => {
    const id = offset
    offset += token.raw.length
    return { id, html: marked.parser([token], { ...MARKED_OPTIONS, renderer }) }
  })
}
