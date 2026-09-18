import * as m from '$lib/paraglide/messages.js'
import type { Locale } from '$lib/localization.svelte'
import type { VendorOptionField } from '$lib/types'

export interface ProviderOptionLabel {
  label: string
  hint?: string
}

type OptionLabelEntry = { label: (locale: Locale) => string; hint?: (locale: Locale) => string }

const VENDOR_OPTION_LABELS: Record<string, Record<string, OptionLabelEntry>> = {
  'command-code': {
    zdr: {
      label: (locale) => m.provider_option_zero_data_retention({}, { locale }),
      hint: (locale) => m.provider_option_zero_data_retention_hint({}, { locale }),
    },
  },
}

/**
 * Resolves built-in vendor option labels through the WebUI catalog. Backend
 * metadata remains the English fallback for a newer unknown option so an
 * older WebUI never selects a misleading translation.
 */
export function providerOptionLabel(vendorId: string, field: VendorOptionField, locale: Locale): ProviderOptionLabel {
  const entry = VENDOR_OPTION_LABELS[vendorId]?.[field.key]
  if (!entry) {
    return { label: field.label }
  }
  return { label: entry.label(locale), hint: entry.hint?.(locale) }
}
