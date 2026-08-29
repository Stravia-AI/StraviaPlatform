const catalogPaths = {
  'en-US': new URL('../messages/en-US.json', import.meta.url),
  'zh-CN': new URL('../messages/zh-CN.json', import.meta.url),
} as const

const metadataKeys: Record<string, true> = { $schema: true }

async function readCatalog(locale: keyof typeof catalogPaths): Promise<Record<string, unknown>> {
  const catalog = (await Bun.file(catalogPaths[locale]).json()) as Record<string, unknown>
  const invalidKeys = Object.entries(catalog)
    .filter(([key, value]) => !metadataKeys[key] && typeof value !== 'string' && !Array.isArray(value))
    .map(([key]) => key)

  if (invalidKeys.length > 0) {
    throw new Error(`${locale} catalog values must be messages: ${invalidKeys.join(', ')}`)
  }

  return Object.fromEntries(Object.entries(catalog).filter(([key]) => !metadataKeys[key]))
}

const [english, chinese] = await Promise.all([readCatalog('en-US'), readCatalog('zh-CN')])
const missingChinese = Object.keys(english).filter((key) => !(key in chinese))
const missingEnglish = Object.keys(chinese).filter((key) => !(key in english))

if (missingChinese.length > 0 || missingEnglish.length > 0) {
  const problems = [
    missingChinese.length > 0 ? `Missing from zh-CN: ${missingChinese.join(', ')}` : '',
    missingEnglish.length > 0 ? `Missing from en-US: ${missingEnglish.join(', ')}` : '',
  ].filter(Boolean)
  throw new Error(problems.join('\n'))
}
