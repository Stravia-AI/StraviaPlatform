export type OAuthCallbackMode = 'auto' | 'manual'

export interface OAuthSessionInitData {
  session_id: string
  vendor: string
  scheme: string
  auth_url: string
  user_code?: string | null
  callback_mode: OAuthCallbackMode
  listener_state: string
  listener_port?: number | null
  redirect_uri: string
  fallback_reason?: string | null
  expires_in: number
  interval: number
}

export type OAuthSessionStatusData =
  | {
      status: 'pending'
      scheme: string
      auth_url: string
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
