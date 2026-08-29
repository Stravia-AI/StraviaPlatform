export interface Model {
  id: string
  name: string
  balance: ModelBalance
  target_provider: string
  target_model: string
  is_enabled: boolean
  created_at: string
  supported_thinking_levels: ThinkingLevel[]
  context_window?: number | null
  output_max_tokens?: number | null
  targets: ModelBackend[]
}

export type ModelBalance = 'weighted' | 'priority'

export interface ModelBackend {
  id: string
  model_id: string
  provider_id: string
  model: string
  weight: number
  priority: number
  created_at: string
  thinking_level_map: ThinkingLevelMapping[]
}

export type ThinkingLevel = 'off' | 'minimal' | 'low' | 'medium' | 'high' | 'xhigh' | 'max'

export type TargetThinkingControl =
  | { type: 'effort'; value: string }
  | { type: 'budget'; value: number }
  | { type: 'enabled' }
  | { type: 'disabled' }
  | { type: 'hidden' }

export interface ThinkingLevelMapping {
  level: ThinkingLevel
  control: TargetThinkingControl
  source: 'generated' | 'overridden'
}

export interface CreateModel {
  name: string
  balance?: ModelBalance
  target_provider: string
  target_model: string
  targets?: CreateModelBackend[]
}

export interface BindRouteInput {
  route_id?: string
  provider_id: string
  provider_model_id: string
  weight?: number
  priority?: number
}

export interface UnbindRouteInput {
  route_id: string
  provider_id: string
  provider_model_id: string
}

export interface UpdateModel {
  name?: string
  balance?: ModelBalance
  target_provider?: string
  target_model?: string
  targets?: UpsertModelBackend[]
  is_enabled?: boolean
}

export interface CreateModelBackend {
  provider_id: string
  model: string
  weight?: number
  priority?: number
  thinking_level_map?: ThinkingLevelMapping[]
}

export interface UpsertModelBackend {
  id?: string
  provider_id: string
  model: string
  weight?: number
  priority?: number
  thinking_level_map?: ThinkingLevelMapping[]
}
