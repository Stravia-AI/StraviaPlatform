import * as m from '#lib/paraglide/messages.js'
import type { ProviderOAuthStatus } from '#lib/types.js'

/** Provider 列表与详情共用同一套本地化 OAuth 凭据状态文案。 */
export function providerOAuthStatusLabel(status: ProviderOAuthStatus): string {
  switch (status) {
    case 'not_connected':
      return m.provider_connection_oauth_not_connected()
    case 'pending':
      return m.provider_connection_oauth_pending()
    case 'connected':
      return m.provider_connection_oauth_connected()
    case 'unavailable':
      return m.common_unavailable()
    case 'quota_exhausted':
      return m.provider_connection_oauth_quota_exhausted()
    case 'error':
      return m.provider_connection_oauth_error()
    case 'disconnected':
      return m.provider_connection_oauth_disconnected()
  }
}
