import { createContext } from 'svelte'

import { ProductUpdateController } from '$lib/product-update'
import type {
  AvailableUpdate,
  DesktopUpdateBridge,
  ProductUpdateCoordinatorSnapshot,
  ProductUpdateViewState,
  UpdateApi,
  UpdateCheckStatus,
  UpdateStatus,
} from '$lib/product-update'

export type {
  AvailableUpdate,
  DesktopUpdateBridge,
  DesktopUpdateProgress,
  DesktopUpdateSnapshot,
  ProductUpdatePhase,
  ProductUpdateViewState,
  UpdateApi,
  UpdateCheckStatus,
  UpdateFailure,
  UpdateStatus,
} from '$lib/product-update'
export { createDesktopUpdateBridge } from '$lib/product-update'

const initialViewState: ProductUpdateViewState = {
  phase: 'idle',
  targetVersion: null,
  downloadedBytes: 0,
  totalBytes: null,
  error: null,
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

  private applySnapshot(snapshot: ProductUpdateCoordinatorSnapshot): void {
    this.status = snapshot.status
    this.state = snapshot.state
    this.notification = snapshot.notification
  }
}

export const [getProductUpdateCoordinator, setProductUpdateCoordinator] =
  createContext<ProductUpdateCoordinator>()
