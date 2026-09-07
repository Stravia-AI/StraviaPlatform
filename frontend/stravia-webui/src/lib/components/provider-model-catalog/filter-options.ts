import * as m from '$lib/paraglide/messages.js'
import { localeState } from '$lib/localization.svelte'

export const allCatalogFilterValue = 'all'

export function catalogFilterOptions() {
  void localeState.current
  return {
    availability: {
      allLabel: m.common_all_models(),
      options: [
        { value: 'available', label: m.common_used() },
        { value: 'unavailable', label: m.common_unavailable() },
      ],
    },
    source: {
      allLabel: m.provider_model_catalog_all_sources(),
      options: [
        { value: 'discovered', label: m.common_synced() },
        { value: 'manual', label: m.common_added_manually() },
      ],
    },
    reference: {
      allLabel: m.provider_model_catalog_all_usage(),
      options: [
        { value: 'referenced', label: m.provider_model_catalog_use() },
        { value: 'unreferenced', label: m.provider_model_catalog_not_use() },
      ],
    },
  }
}
