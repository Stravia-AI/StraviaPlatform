/// <reference lib="webworker" />

import { layoutForest, type CachedLayout, type LayoutRequest, type LayoutResponse } from './interaction-layout'

export type { LayoutPosition, LayoutRequest, LayoutResponse } from './interaction-layout'

const layouts = new Map<string, CachedLayout>()

self.onmessage = (message: MessageEvent<LayoutRequest>) => {
  const response: LayoutResponse = {
    requestId: message.data.requestId,
    positions: layoutForest(message.data, layouts),
  }
  self.postMessage(response)
}
