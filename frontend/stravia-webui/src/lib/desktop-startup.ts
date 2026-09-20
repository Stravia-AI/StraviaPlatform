import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

export type DesktopStartupStatus = 'starting' | 'ready' | 'failed'
export type DesktopStartupStage = 'data_directory' | 'gateway' | 'session' | 'http' | 'desktop'
export type DesktopStartupWarningCode = 'autostart' | 'tray' | 'icons' | 'diagnostics'

export interface DesktopStartupIssue {
  code: string
  details: string
}

export interface DesktopStartupWarning extends DesktopStartupIssue {
  code: DesktopStartupWarningCode
}

export interface DesktopStartupState {
  status: DesktopStartupStatus
  stage: DesktopStartupStage
  error: DesktopStartupIssue | null
  warnings: DesktopStartupWarning[]
  logPath: string | null
  previousFailure: boolean
}

export const initialDesktopStartupState: DesktopStartupState = {
  status: 'starting',
  stage: 'desktop',
  error: null,
  warnings: [],
  logPath: null,
  previousFailure: false,
}

export function desktopFrontendFailure(stage: DesktopStartupStage, details: string): DesktopStartupState {
  return {
    status: 'failed',
    stage,
    error: { code: stage, details },
    warnings: [],
    logPath: null,
    previousFailure: false,
  }
}

function invokeDesktop<T>(command: string): Promise<T> {
  return invoke<T>(command)
}

export async function connectDesktopStartup(onState: (state: DesktopStartupState) => void): Promise<() => void> {
  let eventVersion = 0
  const unlisten = await listen<DesktopStartupState>('desktop-startup-changed', (event) => {
    eventVersion += 1
    onState(event.payload)
  })

  try {
    const versionBeforeSnapshot = eventVersion
    const snapshot = await invokeDesktop<DesktopStartupState>('get_desktop_startup_state')
    if (eventVersion === versionBeforeSnapshot) onState(snapshot)
    return unlisten
  } catch (error) {
    unlisten()
    throw error
  }
}

export function restartDesktop(): Promise<void> {
  return invokeDesktop('restart_desktop')
}

export function exitDesktop(): Promise<void> {
  return invokeDesktop('exit_desktop')
}

export function openDesktopLogs(): Promise<void> {
  return invokeDesktop('open_desktop_logs')
}
