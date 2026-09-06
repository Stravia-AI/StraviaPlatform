export type UpdateCheckStatus = 'idle' | 'up-to-date' | 'available' | 'error'
export type ProductUpdatePhase =
  | 'idle'
  | 'checking'
  | 'up-to-date'
  | 'available'
  | 'downloading'
  | 'downloaded'
  | 'installing'
  | 'error'

export interface AvailableUpdate {
  version: string
  published_at: string
  release_url: string
  manifest_url: string
  download_available: boolean
  download_error: string | null
}

export interface UpdateFailure {
  code: string
  message: string
  attempted_at: string
}

export interface UpdateStatus {
  current_version: string
  check_status: UpdateCheckStatus
  last_success_at: string | null
  last_failure: UpdateFailure | null
  available_update: AvailableUpdate | null
  skipped: boolean
  download_supported: boolean
}

export interface DesktopUpdateSnapshot {
  phase: 'idle' | 'downloading' | 'downloaded' | 'installing' | 'error'
  target_version: string | null
  downloaded_bytes: number
  total_bytes: number | null
  error: string | null
}

export interface DesktopUpdateProgress {
  target_version: string
  downloaded_bytes: number
  total_bytes: number | null
  finished: boolean
}

export interface UpdateApi {
  get(): Promise<UpdateStatus>
  check(mode: 'automatic' | 'manual'): Promise<UpdateStatus>
  skip(version: string | null): Promise<UpdateStatus>
}

export interface DesktopUpdateBridge {
  snapshot(): Promise<DesktopUpdateSnapshot>
  download(version: string): Promise<DesktopUpdateSnapshot>
  install(): Promise<void>
  onProgress(listener: (progress: DesktopUpdateProgress) => void): Promise<() => void>
}

export interface ProductUpdateViewState {
  phase: ProductUpdatePhase
  targetVersion: string | null
  downloadedBytes: number
  totalBytes: number | null
  error: string | null
  installPromptOpen: boolean
  downloadedReleaseUrl: string | null
}

export interface ProductUpdateCoordinatorSnapshot {
  status: UpdateStatus | null
  state: ProductUpdateViewState
  notification: AvailableUpdate | null
}

type SnapshotListener = (snapshot: ProductUpdateCoordinatorSnapshot) => void

const initialViewState: ProductUpdateViewState = {
  phase: 'idle',
  targetVersion: null,
  downloadedBytes: 0,
  totalBytes: null,
  error: null,
  installPromptOpen: false,
  downloadedReleaseUrl: null,
}

export function supportsInAppInstallProgress(userAgent: string): boolean {
  return !userAgent.includes('Windows')
}

export class ProductUpdateController {
  status: UpdateStatus | null = null
  state: ProductUpdateViewState = initialViewState
  notification: AvailableUpdate | null = null

  readonly #api: UpdateApi
  readonly #desktop: DesktopUpdateBridge | null
  readonly #listener: SnapshotListener
  readonly #notifiedVersions = new Set<string>()
  #automaticStarted = false
  #checkInFlight: Promise<UpdateStatus> | null = null

  constructor(
    api: UpdateApi,
    desktop: DesktopUpdateBridge | null = null,
    listener: SnapshotListener = () => undefined,
  ) {
    this.#api = api
    this.#desktop = desktop
    this.#listener = listener
  }

  async load(): Promise<void> {
    try {
      this.applyStatus(await this.#api.get())
    } catch (error) {
      this.state = { ...initialViewState, phase: 'error', error: errorMessage(error) }
      this.publish()
    }
  }

  async automaticCheck(): Promise<void> {
    if (this.#automaticStarted) return
    this.#automaticStarted = true
    try {
      const status = await this.check('automatic')
      const available = status.available_update
      if (
        status.check_status === 'available' &&
        available &&
        !status.skipped &&
        !this.#notifiedVersions.has(available.version)
      ) {
        this.#notifiedVersions.add(available.version)
        this.notification = available
        this.publish()
      }
    } catch {
      // Automatic checks are deliberately silent; Settings retains the failure state.
    }
  }

  async manualCheck(): Promise<UpdateCheckStatus> {
    try {
      return (await this.check('manual')).check_status
    } catch (error) {
      this.state = { ...this.state, phase: 'error', error: errorMessage(error) }
      this.publish()
      return 'error'
    }
  }

  async skipAvailableVersion(): Promise<void> {
    const version = this.status?.available_update?.version
    if (!version) return
    this.applyStatus(await this.#api.skip(version))
    this.notification = null
    this.publish()
  }

  async clearSkippedVersion(): Promise<void> {
    this.applyStatus(await this.#api.skip(null))
  }

  dismissNotification(): void {
    this.notification = null
    this.publish()
  }

  async connectDesktopBridge(): Promise<() => void> {
    if (!this.#desktop) return () => undefined
    this.applyDesktopSnapshot(await this.#desktop.snapshot())
    return this.#desktop.onProgress((progress) => {
      if (this.state.phase !== 'downloading' || this.state.targetVersion !== progress.target_version) return
      this.state = {
        ...this.state,
        downloadedBytes: progress.downloaded_bytes,
        totalBytes: progress.total_bytes,
      }
      this.publish()
    })
  }

  async downloadAvailableUpdate(): Promise<void> {
    const update = this.status?.available_update
    if (!this.#desktop || !update || !this.status?.download_supported || !update.download_available) return
    this.notification = null
    this.state = {
      phase: 'downloading',
      targetVersion: update.version,
      downloadedBytes: 0,
      totalBytes: null,
      error: null,
      installPromptOpen: false,
      downloadedReleaseUrl: update.release_url,
    }
    this.publish()
    try {
      this.applyDesktopSnapshot(await this.#desktop.download(update.version))
    } catch (error) {
      this.state = { ...this.state, phase: 'error', error: errorMessage(error) }
      this.publish()
    }
  }

  async installDownloadedUpdate(): Promise<void> {
    if (!this.#desktop || this.state.phase !== 'downloaded') return
    this.state = { ...this.state, phase: 'installing', error: null, installPromptOpen: false }
    this.publish()
    try {
      await this.#desktop.install()
    } catch (error) {
      this.state = { ...this.state, phase: 'downloaded', error: errorMessage(error) }
      this.publish()
    }
  }

  requestInstallPrompt(): void {
    if (this.state.phase !== 'downloaded') return
    this.state = { ...this.state, installPromptOpen: true }
    this.publish()
  }

  dismissInstallPrompt(): void {
    if (!this.state.installPromptOpen) return
    this.state = { ...this.state, installPromptOpen: false }
    this.publish()
  }

  private async check(mode: 'automatic' | 'manual'): Promise<UpdateStatus> {
    if (this.#checkInFlight) return this.#checkInFlight
    if (!['downloading', 'downloaded', 'installing'].includes(this.state.phase)) {
      this.state = { ...this.state, phase: 'checking', error: null }
      this.publish()
    }
    const request = this.#api.check(mode)
    this.#checkInFlight = request
    try {
      const status = await request
      this.applyStatus(status)
      return status
    } finally {
      this.#checkInFlight = null
    }
  }

  private applyStatus(status: UpdateStatus): void {
    this.status = status
    if (!['downloading', 'downloaded', 'installing'].includes(this.state.phase)) {
      this.state = {
        ...initialViewState,
        phase: status.check_status,
        targetVersion: status.available_update?.version ?? null,
      }
    }
    this.publish()
  }

  private applyDesktopSnapshot(snapshot: DesktopUpdateSnapshot): void {
    if (snapshot.phase === 'idle') return
    const downloadedReleaseUrl =
      this.state.targetVersion === snapshot.target_version
        ? this.state.downloadedReleaseUrl
        : this.status?.available_update?.version === snapshot.target_version
          ? this.status.available_update.release_url
          : null
    this.state = {
      phase: snapshot.phase,
      targetVersion: snapshot.target_version,
      downloadedBytes: snapshot.downloaded_bytes,
      totalBytes: snapshot.total_bytes,
      error: snapshot.error,
      installPromptOpen: snapshot.phase === 'downloaded',
      downloadedReleaseUrl,
    }
    this.publish()
  }

  private publish(): void {
    this.#listener({
      status: this.status,
      state: this.state,
      notification: this.notification,
    })
  }
}

export function createDesktopUpdateBridge(): DesktopUpdateBridge {
  return {
    snapshot: async () => {
      const { invoke } = await import('@tauri-apps/api/core')
      return invoke<DesktopUpdateSnapshot>('get_desktop_update_state')
    },
    download: async (version) => {
      const { invoke } = await import('@tauri-apps/api/core')
      return invoke<DesktopUpdateSnapshot>('download_product_update', { version })
    },
    install: async () => {
      const { invoke } = await import('@tauri-apps/api/core')
      await invoke<void>('install_product_update')
    },
    onProgress: async (listener) => {
      const { listen } = await import('@tauri-apps/api/event')
      return listen<DesktopUpdateProgress>('stravia://product-update-progress', (event) => listener(event.payload))
    },
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
