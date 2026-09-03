import { admin, isTauri } from '$lib/admin-client'
import type { ConnectClientApplyRequest } from '$lib/connect'

export interface ConnectClientApplyPlan {
  paths: string[]
  preview: string
}

export interface ConnectClientApplyError {
  code: string
  message: string
  path?: string
}

export function planConnectClient(input: ConnectClientApplyRequest): Promise<ConnectClientApplyPlan> {
  if (!isTauri) return admin.connectClients.preview(input)
  return import('@tauri-apps/api/core').then(({ invoke }) =>
    invoke<ConnectClientApplyPlan>('plan_connect_client', { input }),
  )
}

export function applyConnectClient(input: ConnectClientApplyRequest): Promise<ConnectClientApplyPlan> {
  if (!isTauri) throw new Error('Connect Client Apply is available only in Stravia Desktop')
  return import('@tauri-apps/api/core').then(({ invoke }) =>
    invoke<ConnectClientApplyPlan>('apply_connect_client', { input }),
  )
}

export function asConnectClientApplyError(error: unknown): ConnectClientApplyError {
  if (typeof error === 'object' && error !== null) {
    const candidate = error as Partial<ConnectClientApplyError>
    if (typeof candidate.code === 'string' && typeof candidate.message === 'string') {
      return {
        code: candidate.code,
        message: candidate.message,
        ...(typeof candidate.path === 'string' ? { path: candidate.path } : {}),
      }
    }
  }
  return { code: 'unknown_error', message: error instanceof Error ? error.message : String(error) }
}
