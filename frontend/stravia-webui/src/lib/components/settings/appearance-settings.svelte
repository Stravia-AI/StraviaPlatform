<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { setMode, userPrefersMode } from 'mode-watcher'

import { localeState } from '#lib/localization.svelte.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import * as Select from '#lib/components/ui/select/index.js'

const currentTheme = $derived(userPrefersMode.current ?? 'system')

function handleThemeValueChange(value: string): void {
  if (value === 'system' || value === 'light' || value === 'dark') setMode(value)
}

function setSelectedLocale(value: string): void {
  if (value === 'en-US' || value === 'zh-CN') localeState.set(value)
}
</script>

<SettingsSection id="appearance" title={m.settings_appearance()}>
  <SettingRow title={m.settings_theme()} labelFor="theme-preference" controlSize="setting">
    <Select.Root type="single" value={currentTheme} onValueChange={handleThemeValueChange}>
      <Select.Trigger id="theme-preference" class="w-full" aria-label={m.settings_theme_preference()}>
        {currentTheme === 'system'
          ? m.settings_follow_system_label()
          : currentTheme === 'light'
            ? m.settings_light()
            : m.settings_dark()}
      </Select.Trigger>
      <Select.Content>
        <Select.Group>
          <Select.Item value="system" label={m.settings_follow_system_label()}>
            {m.settings_follow_system_label()}
          </Select.Item>
          <Select.Item value="light" label={m.settings_light()}>{m.settings_light()}</Select.Item>
          <Select.Item value="dark" label={m.settings_dark()}>{m.settings_dark()}</Select.Item>
        </Select.Group>
      </Select.Content>
    </Select.Root>
  </SettingRow>
  <SettingRow title={m.locale_language_label()} labelFor="interface-language" controlSize="setting">
    <Select.Root type="single" value={localeState.current} onValueChange={setSelectedLocale}>
      <Select.Trigger id="interface-language" class="w-full" aria-label={m.locale_language_label()}>
        {localeState.current === 'zh-CN' ? m.locale_simplified_chinese_autonym() : m.locale_english_autonym()}
      </Select.Trigger>
      <Select.Content>
        <Select.Group>
          <Select.Item value="en-US" label={m.locale_english_autonym()}>{m.locale_english_autonym()}</Select.Item>
          <Select.Item value="zh-CN" label={m.locale_simplified_chinese_autonym()}>
            {m.locale_simplified_chinese_autonym()}
          </Select.Item>
        </Select.Group>
      </Select.Content>
    </Select.Root>
  </SettingRow>
</SettingsSection>
