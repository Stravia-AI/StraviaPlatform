export type Edition = 'ce' | 'commercial'

export type LicenseTier = 'pro' | 'enterprise'

export type LicenseStatus = 'valid' | 'missing' | 'expired' | 'invalid'

export interface CapabilitiesResponse {
  edition: Edition
  license_tier: LicenseTier | null
  license_status: LicenseStatus | null
  capabilities: string[]
  degraded_reason: string | null
  auth_mode: string
}
