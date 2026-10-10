import { describe, expect, test } from 'bun:test'
import { rejects } from 'node:assert/strict'
import { markdownBlocks, renderMarkdownFragment } from '../src/lib/markdown'

function html(text: string): string {
  return markdownBlocks(text)
    .map((block) => block.html)
    .join('')
}

describe('markdown HTML', () => {
  test('recognizes all math delimiters without interpreting ordinary code or escaped dollars', () => {
    const blocks = markdownBlocks('Inline $x^2$ and \\(y\\).\n\n$$z$$\n\n\\[w\\]\n\n`$code$` and \\$5.')
    expect(
      blocks
        .flatMap((block) => block.fragments)
        .map((fragment) => ({
          kind: fragment.kind,
          source: fragment.source,
          display: fragment.display,
          complete: fragment.complete,
        })),
    ).toEqual([
      { kind: 'math', source: 'x^2', display: false, complete: true },
      { kind: 'math', source: 'y', display: false, complete: true },
      { kind: 'math', source: 'z', display: true, complete: true },
      { kind: 'math', source: 'w', display: true, complete: true },
    ])
    expect(blocks.map((block) => block.html).join('')).toContain('<code>$code$</code>')
  })

  test('preserves incomplete and invalid source while distinguishing complete declared diagrams', () => {
    const incompleteMath = markdownBlocks('Before \\[x + y')[0]
    expect(incompleteMath.fragments[0]).toMatchObject({ raw: '\\[x + y', complete: false })
    expect(incompleteMath.html).toContain('\\[x + y')
    const unfinished = markdownBlocks('```mermaid\nflowchart LR\nA --> B')[0]
    expect(unfinished.fragments[0]).toMatchObject({ kind: 'mermaid', complete: false })
    expect(unfinished.html).toContain('flowchart LR')
    expect(unfinished.html).toContain('<pre data-markdown-fragment=')
    const finished = markdownBlocks('```mermaid\nnot a valid diagram\n```')[0]
    expect(finished.fragments[0]).toMatchObject({ source: 'not a valid diagram', complete: true })
    expect(finished.html).toContain('not a valid diagram')
    expect(markdownBlocks('```text\nflowchart LR\nA --> B\n```')[0].fragments).toEqual([])
    expect(markdownBlocks('~~~mermaid\nflowchart LR\nA --> B\n~~~')[0].fragments[0].complete).toBe(true)
  })

  test('rejects diagram resource, configuration and interaction inputs before rendering', async () => {
    for (const source of [
      '%%{init: {"securityLevel":"loose"}}%%\nflowchart LR\nA --> B',
      '---\nconfig:\n  theme: dark\n---\nflowchart LR\nA --> B',
      'flowchart LR\nclick A "https://example.com"',
      'flowchart LR\nA["<img src=x onerror=alert(1)>"]',
      'flowchart LR\nA["&lt;img src=x&gt;"]',
      'flowchart LR\nA@{ img: "https://example.com/image.png" }',
      'flowchart LR\nstyle A fill:url(//example.com/image.svg)',
      'flowchart LR\nclassDef red color:red',
      'flowchart LR\nA["javascript:alert(1)"]',
      String.raw`sequenceDiagram
box \75rl(\68ttps://example.com/image.svg)
participant A
end`,
      'C4Context\nUpdateElementStyle(person, $bgColor="red")',
    ]) {
      const fragment = markdownBlocks(`\`\`\`mermaid\n${source}\n\`\`\``)[0].fragments[0]
      await rejects(() => renderMarkdownFragment(fragment), /Unsupported diagram content/)
    }
  })

  test('rejects trusted TeX extensions and incomplete fragments without invoking a renderer', async () => {
    for (const source of [
      String.raw`\href{https://example.com}{link}`,
      String.raw`\includegraphics{https://example.com/a.png}`,
      String.raw`\htmlStyle{background:url(https://example.com)}{x}`,
      String.raw`\def\evil{\href{https://example.com}{x}}\evil`,
    ]) {
      await rejects(
        () => renderMarkdownFragment(markdownBlocks(`$${source}$`)[0].fragments[0]),
        /Unsupported formula content/,
      )
    }
    await rejects(() => renderMarkdownFragment(markdownBlocks('$x')[0].fragments[0]), /Incomplete Markdown fragment/)
  })

  test('a closed but invalid formula rejects without replacing or discarding its source', async () => {
    const block = markdownBlocks(String.raw`$\frac{a}$`)[0]
    expect(block.fragments[0]).toMatchObject({ source: String.raw`\frac{a}`, complete: true })
    await rejects(() => renderMarkdownFragment(block.fragments[0]), /KaTeX parse error/)
    expect(block.html).toContain(String.raw`$\frac{a}$`)
  })

  test('hides ordinary comments but keeps their neighboring readable text', () => {
    for (const source of [
      'Visible <!-- hidden --> text',
      '<!--hidden-->Visible text<!--hidden-->',
      '<!--hidden-->Visible text<!--unfinished',
    ]) {
      expect(html(source)).toContain('Visible')
      expect(html(source)).toContain('text')
      expect(html(source)).not.toContain('hidden')
      expect(html(source)).not.toContain('unfinished')
    }
  })

  test('keeps block and inline title markup visible as escaped model output', () => {
    for (const { source, escaped } of [
      {
        source: '<title>Analyze the current project codebase</title>',
        escaped: '&lt;title&gt;Analyze the current project codebase&lt;/title&gt;',
      },
      { source: 'Hello <title>x</title> world', escaped: 'Hello &lt;title&gt;x&lt;/title&gt; world' },
    ]) {
      const rendered = html(source)
      expect(rendered).toContain(escaped)
      expect(rendered).not.toContain('<title>')
    }
  })

  test('escapes script and img HTML so they cannot execute', () => {
    expect(html('<script>alert(1)</script>')).toContain('&lt;script&gt;alert(1)&lt;/script&gt;')
    expect(html('<img src="x" onerror="alert(1)">')).toContain(
      '&lt;img src=&quot;x&quot; onerror=&quot;alert(1)&quot;&gt;',
    )
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
