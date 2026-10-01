import type { StartupProgress } from '$lib/startup-progress'

export interface ServerStartupState {
  status: 'starting' | 'ready' | 'failed'
  progress: StartupProgress | null
}

export const initialServerStartupState: ServerStartupState = { status: 'starting', progress: null }

function parseSnapshot(value: unknown): ServerStartupState {
  if (typeof value !== 'object' || value === null) throw new Error('Invalid startup snapshot')
  const state = value as ServerStartupState
  if (!['starting', 'ready', 'failed'].includes(state.status)) throw new Error('Invalid startup status')
  const progress = state.progress
  if (
    progress !== null &&
    (typeof progress !== 'object' ||
      !progress ||
      typeof progress.phase !== 'string' ||
      typeof progress.label !== 'string' ||
      !Number.isFinite(progress.completed) ||
      progress.completed < 0 ||
      (progress.total !== null && (!Number.isFinite(progress.total) || progress.total < 0)))
  )
    throw new Error('Invalid startup progress')
  return { status: state.status, progress }
}

export function connectServerStartup(
  onState: (state: ServerStartupState) => void,
  onConnectionError: () => void,
): () => void {
  let disposed = false
  let finished = false
  let events: EventSource | undefined
  const abort = new AbortController()
  const fail = () => {
    if (disposed || finished) return
    finished = true
    events?.close()
    onConnectionError()
  }
  const apply = (state: ServerStartupState) => {
    if (disposed || finished) return
    if (state.status !== 'starting') {
      finished = true
      events?.close()
    }
    onState(state)
  }

  void (async () => {
    try {
      const response = await fetch('/api/v1/startup', {
        signal: abort.signal,
        credentials: 'same-origin',
        cache: 'no-store',
      })
      if (disposed) return
      // Prepared-app HTTP adapters do not own a startup lifecycle. Only a genuine
      // missing endpoint is compatible; network errors and 503 must remain visible.
      if (response.status === 404) {
        apply({ status: 'ready', progress: null })
        return
      }
      if (!response.ok) throw new Error('Startup snapshot unavailable')
      const snapshot = parseSnapshot(await response.json())
      if (disposed) return
      apply(snapshot)
      if (snapshot.status !== 'starting' || disposed) return
      events = new EventSource('/api/v1/startup/events')
      events.addEventListener('startup', (event) => {
        if (disposed || finished) return
        try {
          apply(parseSnapshot(JSON.parse((event as MessageEvent<string>).data)))
        } catch {
          fail()
        }
      })
      // The stream sends its current snapshot first, covering readiness between
      // the GET and subscription without applying a stale GET after an event.
      events.onerror = fail
    } catch {
      fail()
    }
  })()

  return () => {
    disposed = true
    abort.abort()
    events?.close()
  }
}
