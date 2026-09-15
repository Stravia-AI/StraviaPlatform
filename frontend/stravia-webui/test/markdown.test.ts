import { describe, expect, test } from 'bun:test'
import { markdownBlocks } from '../src/lib/markdown'

function html(text: string): string {
  return markdownBlocks(text)
    .map((block) => block.html)
    .join('')
}

describe('markdown HTML', () => {
  test('shows title-only model output as text instead of swallowing the HTML tag', () => {
    const rendered = html('<title>Analyze the current project codebase</title>')
    expect(rendered).toContain('&lt;title&gt;Analyze the current project codebase&lt;/title&gt;')
    expect(rendered).not.toContain('<title>')
  })

  test('keeps inline title tags visible inside a paragraph', () => {
    const rendered = html('Hello <title>x</title> world')
    expect(rendered).toBe('<p>Hello &lt;title&gt;x&lt;/title&gt; world</p>\n')
  })

  test('escapes script and img HTML so they cannot execute', () => {
    expect(html('<script>alert(1)</script>')).toContain('&lt;script&gt;alert(1)&lt;/script&gt;')
    expect(html('<img src="x" onerror="alert(1)">')).toContain('&lt;img src=&quot;x&quot; onerror=&quot;alert(1)&quot;&gt;')
    expect(html('<script>alert(1)</script>')).not.toContain('<script>')
    expect(html('<img src="x" onerror="alert(1)">')).not.toContain('<img')
  })

  test('still renders GFM emphasis, code, and tables', () => {
    expect(html('**bold** and `code`')).toContain('<strong>bold</strong>')
    expect(html('**bold** and `code`')).toContain('<code>code</code>')
    expect(html('| 项目 | 详情 |\n| --- | --- |\n| 处理器 | **AMD** |')).toContain('<table>')
    expect(html('| 项目 | 详情 |\n| --- | --- |\n| 处理器 | **AMD** |')).toContain('<strong>AMD</strong>')
  })
})
