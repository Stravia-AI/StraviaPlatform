<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { setMode, userPrefersMode } from 'mode-watcher'

import { localeState, type Locale } from '#lib/localization.svelte.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import { SegmentedControl, type SegmentedControlOption } from '#lib/components/ui/segmented-control/index.js'

type ThemePreference = 'system' | 'light' | 'dark'

const currentTheme = $derived<ThemePreference>(userPrefersMode.current ?? 'system')

const themeOptions = $derived<readonly SegmentedControlOption<ThemePreference>[]>([
  { value: 'system', label: m.settings_follow_system_label() },
  { value: 'light', label: m.settings_light() },
  { value: 'dark', label: m.settings_dark() },
])

const languageOptions = $derived<readonly SegmentedControlOption<Locale>[]>([
  { value: 'en-US', label: m.locale_english_autonym() },
  { value: 'zh-CN', label: m.locale_simplified_chinese_autonym() },
])

function handleThemeValueChange(value: ThemePreference): void {
  setMode(value)
}

function setSelectedLocale(value: Locale): void {
  localeState.set(value)
}
</script>

<SettingsSection id="appearance" title={m.settings_appearance()}>
  <SettingRow title={m.settings_theme()}>
    <SegmentedControl
      id="theme-preference"
      options={themeOptions}
      value={currentTheme}
      onValueChange={handleThemeValueChange}
      ariaLabel={m.settings_theme_preference()} />
  </SettingRow>
  <SettingRow title={m.locale_language_label()}>
    <SegmentedControl
      id="interface-language"
      options={languageOptions}
      value={localeState.current}
      onValueChange={setSelectedLocale}
      ariaLabel={m.locale_language_label()} />
  </SettingRow>
</SettingsSection>
