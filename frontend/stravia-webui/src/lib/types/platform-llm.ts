import type { ThinkingLevel } from './route'

export type PlatformLlmStatus = 'unset' | 'available' | 'unavailable'

export interface PlatformLlmRouteSummary {
  id: string
  model_id: string
  display_name: string
}

export interface PlatformLlmConfigView {
  route_id: string | null
  status: PlatformLlmStatus
  route: PlatformLlmRouteSummary | null
  eligible_routes: PlatformLlmRouteSummary[]
}

export interface UpdatePlatformLlmConfig {
  route_id: string | null
}

export interface ConversationTitleInput {
  api_key_id: string
  user_message: string
  assistant_message?: string | null
  locale?: string | null
  thinking_level?: ThinkingLevel | null
}

export interface ConversationTitleResult {
  title: string | null
}
