import { locales, overwriteGetLocale, setLocale, type Locale } from '$lib/paraglide/runtime.js'

export type { Locale }

export const SUPPORTED_LOCALES = locales

const LOCALE_STORAGE_KEY = 'stravia-locale'

function isLocale(value: string): value is Locale {
  return SUPPORTED_LOCALES.some((locale) => locale === value)
}

function detectClientLocale(): Locale {
  const languages = navigator.languages.length > 0 ? navigator.languages : [navigator.language]

  for (const language of languages) {
    try {
      const locale = new Intl.Locale(language).maximize()
      if (locale.language === 'zh' && locale.script === 'Hans') return 'zh-CN'
    } catch {
      // Ignore malformed client locale tags and continue with the next preference.
    }
  }

  return 'en-US'
}

class LocaleState {
  current = $state<Locale>('en-US')

  restore(): void {
    const saved = localStorage.getItem(LOCALE_STORAGE_KEY)
    this.set(saved !== null && isLocale(saved) ? saved : detectClientLocale())
  }

  set(next: Locale, persist = true): void {
    setLocale(next, { reload: false })
    this.current = next
    document.documentElement.lang = next
    if (persist) localStorage.setItem(LOCALE_STORAGE_KEY, next)
  }
}

export const localeState = new LocaleState()
overwriteGetLocale(() => localeState.current)
