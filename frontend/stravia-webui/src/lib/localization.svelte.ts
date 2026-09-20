import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '$lib/auth'
import { locales, overwriteGetLocale, setLocale } from '$lib/paraglide/runtime.js'
import type { Locale } from '$lib/paraglide/runtime.js'

export type { Locale }

export const SUPPORTED_LOCALES = locales

const LOCALE_STORAGE_KEY = 'stravia-locale'

// 桌面托盘菜单等原生文案跟随界面语言；同步是尽力而为，失败不影响界面。
function syncDesktopLocale(locale: Locale): void {
  if (!isTauri) return
  void invoke('set_desktop_locale', { locale }).catch(() => console.warn('Stravia desktop locale sync failed'))
}

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
  #desktopSyncReady = false

  restore(): void {
    const saved = localStorage.getItem(LOCALE_STORAGE_KEY)
    this.set(saved !== null && isLocale(saved) ? saved : detectClientLocale())
  }

  set(next: Locale, persist = true): void {
    void setLocale(next, { reload: false })
    this.current = next
    document.documentElement.lang = next
    if (persist) localStorage.setItem(LOCALE_STORAGE_KEY, next)
    if (this.#desktopSyncReady) syncDesktopLocale(next)
  }

  enableDesktopSync(): void {
    if (!isTauri || this.#desktopSyncReady) return
    this.#desktopSyncReady = true
    syncDesktopLocale(this.current)
  }
}

export const localeState = new LocaleState()
overwriteGetLocale(() => localeState.current)
