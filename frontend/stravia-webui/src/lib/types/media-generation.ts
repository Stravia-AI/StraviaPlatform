export interface MediaGenerationConfig {
  enabled: boolean
  image: { route_id: string | null }
}

export interface MediaGenerationValidation {
  valid: boolean
  code: string | null
  message: string | null
}

export interface MediaGenerationConfigView {
  config: MediaGenerationConfig
  validation: MediaGenerationValidation
}

export interface EligibleMediaGenerationRoute {
  id: string
  name: string | null
}
