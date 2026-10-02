import { admin } from '$lib/admin-client'

export interface RpmPool {
  id: string
  name: string
  rpm_limit: number | null
}

export interface DestinationRpmLimit {
  provider_id: string
  model: string | null
  rpm_limit: number | null
}

export interface RpmConfig {
  preferred_wait_ms: number
  total_wait_ms: number
  queue_capacity: number
  destinations: DestinationRpmLimit[]
  pools: RpmPool[]
}

export const rpmQueryKey = ['setting', 'rpm_admission']

export async function loadRpm(): Promise<RpmConfig> {
  const value = await admin.settings.get('rpm_admission')
  return value === null
    ? { preferred_wait_ms: 5000, total_wait_ms: 30000, queue_capacity: 128, destinations: [], pools: [] }
    : (JSON.parse(value) as RpmConfig)
}

export function saveRpm(config: RpmConfig): Promise<void> {
  return admin.settings.set('rpm_admission', JSON.stringify(config))
}
