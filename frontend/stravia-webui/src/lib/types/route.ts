export interface Route {
  id: string
  model_id: string
  display_name?: string | null
  balance: RouteSelectionStrategy
  target_provider: string
  target_model: string
  is_enabled: boolean
  created_at: string
  default_thinking_level?: ThinkingLevel | null
  supported_thinking_levels: ThinkingLevel[]
  context_window?: number | null
  output_max_tokens?: number | null
  supports_image_input?: boolean
  targets: Target[]
}

export type RouteSelectionStrategy = 'traffic_equalization' | 'latency_preference'

export interface Target {
  id: string
  model_id: string
  provider_id: string
  model: string
  enabled: boolean
  priority: number
  first_token_timeout_ms: number
  target_retry_budget: number
  target_cooldown_ms: number
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

export interface CreateRoute {
  model_id: string
  display_name?: string | null
  balance?: RouteSelectionStrategy
  target_provider: string
  target_model: string
  targets?: CreateTarget[]
  default_thinking_level?: ThinkingLevel | null
}

export interface BindRouteInput {
  route_id?: string
  provider_id: string
  provider_model_id: string
  priority?: number
  first_token_timeout_ms?: number
  target_retry_budget?: number
  target_cooldown_ms?: number
}

export interface UnbindRouteInput {
  route_id: string
  provider_id: string
  provider_model_id: string
}

export interface UpdateRoute {
  model_id?: string
  display_name?: string | null
  balance?: RouteSelectionStrategy
  target_provider?: string
  target_model?: string
  targets?: UpsertTarget[]
  is_enabled?: boolean
  default_thinking_level?: ThinkingLevel | null
}

export interface CreateTarget {
  provider_id: string
  model: string
  enabled?: boolean
  priority?: number
  first_token_timeout_ms?: number
  target_retry_budget?: number
  target_cooldown_ms?: number
  thinking_level_map?: ThinkingLevelMapping[]
}

export interface UpsertTarget {
  id?: string
  provider_id: string
  model: string
  enabled?: boolean
  priority?: number
  first_token_timeout_ms?: number
  target_retry_budget?: number
  target_cooldown_ms?: number
  thinking_level_map?: ThinkingLevelMapping[]
}

export type TargetRuntimeState = 'available' | 'cooling_down' | 'half_open' | 'probing'

export interface TargetRuntimeStatus {
  target_id: string
  provider_id: string
  model: string
  state: TargetRuntimeState
  cooldown_remaining_ms: number | null
}
