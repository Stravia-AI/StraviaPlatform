import { createContext } from 'svelte'

import { ProductUpdateController } from '#lib/product-update.js'
import type {
  AvailableUpdate,
  DesktopUpdateBridge,
  ProductUpdateCoordinatorSnapshot,
  ProductUpdateViewState,
  UpdateApi,
  UpdateChannel,
  UpdateCheckStatus,
  UpdateStatus,
} from '#lib/product-update.js'

export type {
  AvailableUpdate,
  DesktopUpdateBridge,
  DesktopUpdateProgress,
  DesktopUpdateSnapshot,
  ProductUpdatePhase,
  ProductUpdateViewState,
  UpdateApi,
  UpdateChannel,
  UpdateCheckStatus,
  UpdateFailure,
  UpdateStatus,
} from '#lib/product-update.js'
export { createDesktopUpdateBridge, isPrereleaseVersion } from '#lib/product-update.js'

const initialViewState: ProductUpdateViewState = {
  phase: 'idle',
  targetVersion: null,
  downloadedBytes: 0,
  totalBytes: null,
  error: null,
  installPromptOpen: false,
  downloadedReleaseUrl: null,
}

export class ProductUpdateCoordinator {
  status = $state.raw<UpdateStatus | null>(null)
  state = $state.raw<ProductUpdateViewState>(initialViewState)
  notification = $state.raw<AvailableUpdate | null>(null)

  readonly #controller: ProductUpdateController

  constructor(api: UpdateApi, desktop: DesktopUpdateBridge | null = null) {
    this.#controller = new ProductUpdateController(api, desktop, (snapshot) => {
      this.applySnapshot(snapshot)
    })
  }

  load(): Promise<void> {
    return this.#controller.load()
  }

  automaticCheck(): Promise<void> {
    return this.#controller.automaticCheck()
  }

  manualCheck(): Promise<UpdateCheckStatus> {
    return this.#controller.manualCheck()
  }

  silentCheck(): Promise<UpdateCheckStatus | null> {
    return this.#controller.silentCheck()
  }

  setChannel(channel: UpdateChannel): Promise<UpdateStatus> {
    return this.#controller.setChannel(channel)
  }

  skipAvailableVersion(): Promise<void> {
    return this.#controller.skipAvailableVersion()
  }

  clearSkippedVersion(): Promise<void> {
    return this.#controller.clearSkippedVersion()
  }

  dismissNotification(): void {
    this.#controller.dismissNotification()
  }

  connectDesktopBridge(): Promise<() => void> {
    return this.#controller.connectDesktopBridge()
  }

  downloadAvailableUpdate(): Promise<void> {
    return this.#controller.downloadAvailableUpdate()
  }

  installDownloadedUpdate(): Promise<void> {
    return this.#controller.installDownloadedUpdate()
  }

  requestInstallPrompt(): void {
    this.#controller.requestInstallPrompt()
  }

  dismissInstallPrompt(): void {
    this.#controller.dismissInstallPrompt()
  }

  private applySnapshot(snapshot: ProductUpdateCoordinatorSnapshot): void {
    this.status = snapshot.status
    this.state = snapshot.state
    this.notification = snapshot.notification
  }
}

export const [getProductUpdateCoordinator, setProductUpdateCoordinator] = createContext<ProductUpdateCoordinator>()
