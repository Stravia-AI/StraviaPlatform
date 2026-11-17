import { afterEach, expect, test } from 'bun:test'
import { authenticatedFetch, restoreAuthentication, type AuthState } from '../src/lib/auth'

const originalFetch = globalThis.fetch
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')
const signedOut: AuthState = { mode: 'server', authenticated: false, setup_authorized: false, username: null }
const signedIn: AuthState = { ...signedOut, authenticated: true, username: 'admin' }

function requestUrl(input: string | URL | Request): string {
  if (typeof input === 'string') return input
  return input instanceof URL ? input.href : input.url
}

function stateResponse(state: AuthState): Response {
  return Response.json(state)
}

function errorResponse(status: number): Response {
  return Response.json({ error: { code: `failure_${status}`, message: 'Request failed' } }, { status })
}

afterEach(() => {
  globalThis.fetch = originalFetch
  if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow)
  else Reflect.deleteProperty(globalThis, 'window')
})

for (const endpoint of ['/auth/state', '/auth/refresh']) {
  for (const status of [503, 403]) {
    test(`restore propagates ${status} from ${endpoint}`, async () => {
      globalThis.fetch = ((input: string | URL | Request) => {
        const path = requestUrl(input)
        return Promise.resolve(path.endsWith(endpoint) ? errorResponse(status) : stateResponse(signedOut))
      }) as typeof fetch
      const error: unknown = await restoreAuthentication(signedOut).catch((error: unknown) => error)
      expect(error).toMatchObject({ status, code: `failure_${status}` })
    })
  }

  test(`restore propagates network errors from ${endpoint}`, async () => {
    const failure = new TypeError('network unavailable')
    globalThis.fetch = ((input: string | URL | Request) => {
      return requestUrl(input).endsWith(endpoint) ? Promise.reject(failure) : Promise.resolve(stateResponse(signedOut))
    }) as typeof fetch
    const error: unknown = await restoreAuthentication(signedOut).catch((error: unknown) => error)
    expect(error).toBe(failure)
  })
}

test('restore refreshes and returns the authenticated state', async () => {
  let refreshed = false
  globalThis.fetch = ((input: string | URL | Request) => {
    if (requestUrl(input).endsWith('/auth/refresh')) {
      refreshed = true
      return Promise.resolve(new Response(null, { status: 204 }))
    }
    return Promise.resolve(stateResponse(refreshed ? signedIn : signedOut))
  }) as typeof fetch
  expect(await restoreAuthentication(signedOut)).toEqual(signedIn)
})

test('a refresh 401 leaves restoration signed out and allows login handling', async () => {
  Object.defineProperty(globalThis, 'window', { configurable: true, value: { location: { pathname: '/login' } } })
  globalThis.fetch = ((input: string | URL | Request) => {
    const path = requestUrl(input)
    if (path.endsWith('/auth/state')) return Promise.resolve(stateResponse(signedOut))
    return Promise.resolve(errorResponse(401))
  }) as typeof fetch
  expect(await restoreAuthentication(signedOut)).toEqual(signedOut)
  expect((await authenticatedFetch('/protected')).status).toBe(401)
})

test('authenticated requests propagate renewal failures instead of requiring login', async () => {
  const failure = errorResponse(503)
  globalThis.fetch = ((input: string | URL | Request) => {
    const path = requestUrl(input)
    if (path.endsWith('/auth/state')) return Promise.resolve(stateResponse(signedOut))
    if (path.endsWith('/auth/refresh')) return Promise.resolve(failure)
    return Promise.resolve(errorResponse(401))
  }) as typeof fetch
  const error: unknown = await authenticatedFetch('/protected').catch((error: unknown) => error)
  expect(error).toMatchObject({ status: 503, code: 'failure_503' })
})

test('concurrent restorations share a single refresh', async () => {
  const started = Promise.withResolvers<void>()
  const refresh = Promise.withResolvers<Response>()
  let refreshCalls = 0
  let refreshed = false
  globalThis.fetch = ((input: string | URL | Request) => {
    if (requestUrl(input).endsWith('/auth/refresh')) {
      refreshCalls++
      started.resolve()
      return refresh.promise
    }
    return Promise.resolve(stateResponse(refreshed ? signedIn : signedOut))
  }) as typeof fetch
  const first = restoreAuthentication(signedOut)
  const second = restoreAuthentication(signedOut)
  await started.promise
  refreshed = true
  refresh.resolve(new Response(null, { status: 204 }))
  expect(await Promise.all([first, second])).toEqual([signedIn, signedIn])
  expect(refreshCalls).toBe(1)
})
