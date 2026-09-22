export type OAuthCallbackMode = 'auto' | 'manual'

export interface OAuthCandidateConfiguration {
  provider_id?: string
  base_url: string
  protocol?: string | null
  options: Record<string, unknown>
  credentials: Record<string, unknown>
}

export interface OAuthManualInput {
  type: 'text' | 'callback_url'
  label: string
  description?: string | null
  secret: boolean
}

export interface OAuthSessionInitData {
  session_id: string
  /** Supplier profile type ID; the wire field name remains `vendor_id`. */
  vendor_id: string
  channel: string
  flow: 'authorization_code' | 'device_code' | 'manual'
  auth_url?: string | null
  user_code?: string | null
  callback_mode: OAuthCallbackMode
  listener_state: string
  listener_port?: number | null
  redirect_uri: string
  fallback_reason?: string | null
  expires_in: number
  interval: number
  manual_input?: OAuthManualInput | null
}

export type OAuthSessionStatusData =
  | {
      status: 'pending'
      auth_url?: string | null
      user_code?: string | null
      callback_mode: OAuthCallbackMode
      listener_state: string
      listener_port?: number | null
      redirect_uri: string
      fallback_reason?: string | null
      error_code?: string | null
      last_error?: string | null
      expires_in: number
      interval: number
      manual_input?: OAuthManualInput | null
    }
  | { status: 'exchanging'; expires_in: number }
  | { status: 'ready'; expires_in: number; resource_url?: string | null }
  | { status: 'error'; code: string; message: string }

export type ProviderOAuthStatus =
  'not_connected' | 'pending' | 'connected' | 'unavailable' | 'quota_exhausted' | 'error' | 'disconnected'

export interface ProviderOAuthStatusData {
  provider_id: string
  provider_name: string
  driver_key: string
  status: ProviderOAuthStatus
  expires_at?: string | null
  resource_url?: string | null
  subject_id?: string | null
  last_error?: string | null
  updated_at?: string | null
  has_refresh_token: boolean
}
