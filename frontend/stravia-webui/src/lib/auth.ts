import * as m from '$lib/paraglide/messages.js'

export type AuthMode = 'setup' | 'server' | 'desktop' | 'unavailable'

export interface AuthState {
  mode: AuthMode
  authenticated: boolean
  setup_authorized: boolean
  username: string | null
}

export interface SessionSummary {
  username: string | null
  access_expires_at: number
  session_expires_at: number
}

export interface DatabaseConfig {
  backend: 'sqlite' | 'postgres'
  path?: string
  url?: string
  max_connections?: number
  min_connections?: number
  idle_timeout_seconds?: number
}

interface NativeSession {
  access_token: string
}

interface ErrorPayload {
  error?: unknown
  code?: string
  params?: Record<string, unknown>
  data?: unknown
}

export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

let desktopApiBase: Promise<string> | undefined
let nativeSession: NativeSession | undefined
let refreshFlight: Promise<boolean> | undefined

export async function apiBase(): Promise<string> {
  if (!isTauri) return '/api/v1'
  // Keep the native bridge out of the browser bundle path; it only exists in the Tauri runtime.
  desktopApiBase ??= import('@tauri-apps/api/core')
    .then(({ invoke }) => invoke<number>('get_server_port'))
    .then((port) => `http://127.0.0.1:${port}/api/v1`)
  return desktopApiBase
}

async function acquireNativeSession(): Promise<NativeSession> {
  // Keep the native bridge out of ordinary Server web sessions.
  const { invoke } = await import('@tauri-apps/api/core')
  nativeSession = await invoke<NativeSession>('get_admin_session')
  return nativeSession
}

async function rawFetch(path: string, init: RequestInit = {}, ensureNative = true): Promise<Response> {
  const headers = new Headers(init.headers)
  if (isTauri && ensureNative) {
    const session = nativeSession ?? (await acquireNativeSession())
    headers.set('Authorization', `Bearer ${session.access_token}`)
  }
  return fetch(`${await apiBase()}${path}`, { ...init, headers, credentials: isTauri ? 'omit' : 'same-origin' })
}

function unsafeHeaders(body: boolean): Headers {
  const headers = new Headers({ 'X-Stravia-CSRF': '1' })
  if (body) headers.set('Content-Type', 'application/json')
  return headers
}

async function decode<T>(response: Response): Promise<T> {
  const text = await response.text()
  const payload = text ? parseJson(text) : undefined
  const object = payload !== null && typeof payload === 'object' ? (payload as ErrorPayload) : undefined
  if (!response.ok) {
    const message = typeof object?.error === 'string' ? object.error : `HTTP ${response.status}`
    const error = new Error(message) as Error & { code?: string; params?: Record<string, unknown>; status?: number }
    error.status = response.status
    if (object?.code) error.code = object.code
    if (object?.params) error.params = object.params
    throw error
  }
  if (typeof object?.error === 'string' && object.error.trim()) throw new Error(object.error)
  return object && 'data' in object ? (object.data as T) : (payload as T)
}

function parseJson(value: string): unknown {
  try {
    return JSON.parse(value)
  } catch {
    throw new Error(m.frontend_error_invalid_admin_json())
  }
}

export async function getAuthState(): Promise<AuthState> {
  const response = await rawFetch('/auth/state', {}, isTauri)
  return decode<AuthState>(response)
}

async function refreshServerSession(): Promise<boolean> {
  const run = async (): Promise<boolean> => {
    const stateResponse = await rawFetch('/auth/state', {}, false)
    if (stateResponse.ok && (await decode<AuthState>(stateResponse)).authenticated) return true
    const response = await rawFetch('/auth/refresh', { method: 'POST', headers: unsafeHeaders(false) }, false)
    return response.ok
  }

  if (typeof navigator !== 'undefined' && navigator.locks) {
    return navigator.locks.request('stravia-admin-session-refresh', { mode: 'exclusive' }, run)
  }
  return run()
}

async function renewAuthentication(): Promise<boolean> {
  refreshFlight ??= (
    isTauri
      ? acquireNativeSession().then(
          () => true,
          () => false,
        )
      : refreshServerSession().catch(() => false)
  ).finally(() => {
    refreshFlight = undefined
  })
  return refreshFlight
}

export async function restoreAuthentication(state: AuthState): Promise<AuthState> {
  if (state.authenticated || (state.mode !== 'server' && state.mode !== 'unavailable')) return state
  if (!(await renewAuthentication())) return state
  return getAuthState()
}

export async function authenticatedFetch(path: string, init: RequestInit = {}): Promise<Response> {
  let response = await rawFetch(path, init)
  if (response.status !== 401) return response
  if (!(await renewAuthentication())) return response
  response = await rawFetch(path, init)
  return response
}

export async function login(username: string, password: string): Promise<SessionSummary> {
  const response = await rawFetch(
    '/auth/login',
    { method: 'POST', headers: unsafeHeaders(true), body: JSON.stringify({ username, password }) },
    false,
  )
  return decode<SessionSummary>(response)
}

export async function logout(): Promise<void> {
  if (isTauri) return
  const response = await authenticatedFetch('/auth/logout', { method: 'POST', headers: unsafeHeaders(false) })
  await decode<void>(response)
}

export async function changeCredentials(currentPassword: string, username: string, password: string): Promise<void> {
  const response = await authenticatedFetch('/auth/credentials', {
    method: 'PUT',
    headers: unsafeHeaders(true),
    body: JSON.stringify({ current_password: currentPassword, username, password }),
  })
  await decode<void>(response)
}

export async function claimSetup(token: string): Promise<void> {
  const response = await rawFetch(
    '/setup/claim',
    { method: 'POST', headers: unsafeHeaders(true), body: JSON.stringify({ token }) },
    false,
  )
  await decode<void>(response)
}

export async function testDatabase(database: DatabaseConfig): Promise<void> {
  const response = await rawFetch(
    '/setup/test',
    { method: 'POST', headers: unsafeHeaders(true), body: JSON.stringify({ database }) },
    false,
  )
  await decode<void>(response)
}

export async function completeSetup(
  database: DatabaseConfig,
  username: string,
  password: string,
  client_base_url: string,
): Promise<AuthState> {
  const response = await rawFetch(
    '/setup/complete',
    {
      method: 'POST',
      headers: unsafeHeaders(true),
      body: JSON.stringify({ database, username, password, client_base_url }),
    },
    false,
  )
  return decode<AuthState>(response)
}

export function authenticationRequired(): never {
  window.location.assign('/login')
  throw new Error(m.frontend_error_authentication_required())
}
