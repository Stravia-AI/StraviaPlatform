import * as m from '$lib/paraglide/messages.js'
import { type Locale } from '$lib/localization.svelte'
import type { ProviderModelSelectionPolicy } from '$lib/types'

export function providerModelSelectionPolicyLabel(policy: ProviderModelSelectionPolicy, locale: Locale): string {
  if (policy === 'force_enabled') return m.common_always_allow({}, { locale: locale })
  if (policy === 'force_disabled') return m.common_don_t_allow({}, { locale: locale })
  return m.common_use_synced_status({}, { locale: locale })
}
