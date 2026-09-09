export interface Route {
  id: string
  model_id: string
  display_name?: string | null
  balance: RouteSelectionStrategy
  target_provider: string
  target_model: string
  is_enabled: boolean
  created_at: string
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
