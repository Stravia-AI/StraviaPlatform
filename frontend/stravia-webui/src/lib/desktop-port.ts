import * as m from '$lib/paraglide/messages.js'

import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '$lib/admin-client'

export type DesktopPortMode = 'fixed' | 'fallback' | 'configError'
export type BindingFailureKind = 'addrInUse' | 'other'
export type OwnerLookupStatus = 'notApplicable' | 'identifying' | 'found' | 'unknown'
export type PortOperationErrorCode = 'invalidPort' | 'bindFailed' | 'storeWriteFailed' | 'noFixedPort'

export interface PortOwner {
  name: string
  pid: number
}

export interface BindingFailure {
  kind: BindingFailureKind
  message: string
}

export interface DesktopPortState {
  currentPort: number
  fixedPort: number | null
  mode: DesktopPortMode
  bindingFailure: BindingFailure | null
  ownerLookup: OwnerLookupStatus
  owners: PortOwner[]
  configError: string | null
  candidatePort: number | null
  candidateError: DesktopPortOperationError | null
}

export interface DesktopPortOperationError {
  code: PortOperationErrorCode
  message: string
  bindingFailure: BindingFailure | null
  ownerLookup: OwnerLookupStatus
  owners: PortOwner[]
}

async function invokeDesktop<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri) throw new Error(m.desktop_port_error_desktop_only())
  return invoke<T>(command, args)
}

export function getDesktopPortState(): Promise<DesktopPortState> {
  return invokeDesktop('get_desktop_port_state')
}

export function setDesktopFixedPort(port: number): Promise<DesktopPortState> {
  return invokeDesktop('set_desktop_fixed_port', { port })
}

export function recheckDesktopFixedPort(): Promise<DesktopPortState> {
  return invokeDesktop('recheck_desktop_fixed_port')
}

export function asDesktopPortOperationError(error: unknown): DesktopPortOperationError | undefined {
  if (!error || typeof error !== 'object') return undefined
  const candidate = error as Partial<DesktopPortOperationError>
  if (typeof candidate.code !== 'string' || typeof candidate.message !== 'string') return undefined
  return {
    code: candidate.code as PortOperationErrorCode,
    message: candidate.message,
    bindingFailure: candidate.bindingFailure ?? null,
    ownerLookup: candidate.ownerLookup ?? 'notApplicable',
    owners: Array.isArray(candidate.owners) ? candidate.owners : [],
  }
}

export { isTauri }
