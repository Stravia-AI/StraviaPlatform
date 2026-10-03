import { createParser, type EventSourceMessage } from 'eventsource-parser'

import { apiBase, authenticatedFetch, isTauri } from '$lib/auth'
import { openExternalUrl } from '$lib/open-external'
import { isLiveContentBlock } from '$lib/observation-state'
import type { DownloadTicket, ObservationChange, ObservationLiveUpdate, ObservationStreamUpdate } from '$lib/types'

export interface ObservationSubscription {
  close(): void
  setCursor(sequence: number): void
}

export function subscribeToObservations(
  snapshotSequence: number,
  onUpdate: (update: ObservationStreamUpdate) => void | Promise<void>,
  onConnectionChange: (connected: boolean) => void,
): ObservationSubscription {
  return subscribeStream(
    snapshotSequence,
    (update) => {
      if (update.type === 'event' || update.type === 'reset_required') return onUpdate(update)
    },
    onConnectionChange,
  )
}

export function subscribeToInteractionLive(
  interactionId: string,
  onUpdate: (update: ObservationLiveUpdate) => void,
  onConnectionChange: (connected: boolean) => void = () => undefined,
): ObservationSubscription {
  return subscribeStream(
    0,
    (update) => {
      if (update.type !== 'event' && update.type !== 'reset_required') onUpdate(update)
    },
    onConnectionChange,
    interactionId,
  )
}

function subscribeStream(
  snapshotSequence: number,
  onUpdate: (update: ObservationStreamUpdate | ObservationLiveUpdate) => void | Promise<void>,
  onConnectionChange: (connected: boolean) => void,
  interactionId?: string,
): ObservationSubscription {
  let cursor = snapshotSequence
  let stopped = false
  let controller: AbortController | undefined
  let reconnectDelay = 500
  let deliveryFailed = false

  const deliver = (update: ObservationStreamUpdate | ObservationLiveUpdate): void => {
    const connection = controller
    const pending = onUpdate(update)
    if (pending) {
      void pending.catch(() => {
        if (stopped || controller !== connection) return
        // A consumer rejection invalidates this connection; reconnect without blocking notifications.
        if (update.type === 'event') cursor = Math.min(cursor, update.event.sequence - 1)
        deliveryFailed = true
        connection?.abort()
      })
    }
  }

  const accept = async (message: EventSourceMessage): Promise<void> => {
    if (
      !message.data ||
      !message.event ||
      !(
        interactionId
          ? ['live_content', 'live_snapshot', 'live_gap', 'live_finished']
          : ['observation', 'reset_required']
      ).includes(message.event)
    )
      return
    const payload: unknown = JSON.parse(message.data)
    if (!payload || typeof payload !== 'object') throw new Error('Invalid Observation event')
    if (message.event.startsWith('live_')) {
      if (message.id) throw new Error('Volatile Observation cannot have a cursor')
      if (message.event === 'live_content') {
        if (!isLiveContentBlock(payload)) throw new Error('Invalid live content')
        deliver({ type: 'live_content', block: payload })
      } else if (message.event === 'live_snapshot') {
        if (!('blocks' in payload) || !Array.isArray(payload.blocks) || !payload.blocks.every(isLiveContentBlock))
          throw new Error('Invalid live snapshot')
        deliver({ type: 'live_snapshot', blocks: payload.blocks })
      } else if (message.event === 'live_finished') {
        if (
          !('interaction_id' in payload) ||
          typeof payload.interaction_id !== 'string' ||
          !('run_id' in payload) ||
          typeof payload.run_id !== 'string'
        )
          throw new Error('Invalid live terminal')
        deliver({ type: 'live_finished', interaction_id: payload.interaction_id, run_id: payload.run_id })
      } else {
        if (
          !('interaction_id' in payload) ||
          typeof payload.interaction_id !== 'string' ||
          !('run_id' in payload) ||
          typeof payload.run_id !== 'string' ||
          !('reason' in payload) ||
          typeof payload.reason !== 'string'
        )
          throw new Error('Invalid live gap')
        deliver({
          type: 'live_gap',
          interaction_id: payload.interaction_id,
          run_id: payload.run_id,
          reason: payload.reason,
        })
      }
      return
    }
    if (message.event === 'reset_required') {
      if (!('snapshot_sequence' in payload) || !Number.isSafeInteger(payload.snapshot_sequence)) {
        throw new Error('Invalid Observation reset sequence')
      }
      await onUpdate({ type: 'reset_required', snapshot_sequence: Number(payload.snapshot_sequence) })
      return
    }
    const event = payload as ObservationChange
    const sequence = message.id ? Number(message.id) : event.sequence
    if (!Number.isSafeInteger(sequence) || sequence !== event.sequence) throw new Error('Invalid Observation sequence')
    if (sequence <= cursor) return
    deliver({ type: 'event', event })
    cursor = Math.max(cursor, sequence)
  }

  const connect = async (): Promise<void> => {
    while (!stopped) {
      controller = new AbortController()
      deliveryFailed = false
      let superseded: boolean
      try {
        const response = await authenticatedFetch(
          interactionId
            ? `/observations/interactions/${encodeURIComponent(interactionId)}/live`
            : `/observations/events?after=${cursor}`,
          { headers: { Accept: 'text/event-stream' }, signal: controller.signal },
        )
        if (!response.ok || !response.body) throw new Error(`Observation stream HTTP ${response.status}`)
        onConnectionChange(true)
        reconnectDelay = 500
        const pending: EventSourceMessage[] = []
        const parser = createParser({ onEvent: (message) => pending.push(message) })
        const reader = response.body.pipeThrough(new TextDecoderStream()).getReader()
        try {
          while (!stopped && !controller.signal.aborted) {
            const { value, done } = await reader.read()
            if (done) break
            parser.feed(value)
            for (const message of pending) {
              if (stopped || controller.signal.aborted) break
              await accept(message)
            }
            pending.length = 0
          }
          superseded = controller.signal.aborted && !deliveryFailed
        } finally {
          reader.releaseLock()
        }
      } catch (error) {
        if (stopped) break
        superseded = !deliveryFailed && error instanceof DOMException && error.name === 'AbortError'
      } finally {
        onConnectionChange(false)
      }
      if (stopped) break
      if (superseded) continue
      const delay = Promise.withResolvers<void>()
      setTimeout(delay.resolve, reconnectDelay)
      await delay.promise
      reconnectDelay = Math.min(reconnectDelay * 2, 10_000)
    }
  }

  void connect()
  return {
    close() {
      stopped = true
      controller?.abort()
    },
    setCursor(sequence: number) {
      if (sequence === cursor) return
      cursor = sequence
      controller?.abort()
    },
  }
}

export async function navigateToBundle(ticket: DownloadTicket): Promise<void> {
  const base = await apiBase()
  const adminOrigin = base.endsWith('/api/v1') ? base.slice(0, -'/api/v1'.length) : base
  const href = ticket.download_url.startsWith('http')
    ? ticket.download_url
    : ticket.download_url.startsWith('/api/v1/')
      ? `${adminOrigin}${ticket.download_url}`
      : `${base}${ticket.download_url.startsWith('/') ? ticket.download_url : `/${ticket.download_url}`}`
  if (isTauri) {
    // 一次性票据可独立下载；交给系统浏览器，避免在内嵌 WebView 中导航。
    await openExternalUrl(href)
    return
  }
  const anchor = document.createElement('a')
  anchor.href = href
  anchor.dataset.sveltekitReload = ''
  anchor.rel = 'noreferrer'
  document.body.append(anchor)
  anchor.click()
  anchor.remove()
}
