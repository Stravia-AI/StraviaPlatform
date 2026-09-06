import { describe, expect, test } from 'bun:test'

import { ProductUpdateController, supportsInAppInstallProgress } from '../src/lib/product-update'
import type {
  DesktopUpdateBridge,
  DesktopUpdateSnapshot,
  UpdateApi,
  UpdateStatus,
} from '../src/lib/product-update'

const available: UpdateStatus = {
  current_version: '1.0.0',
  check_status: 'available',
  last_success_at: '2026-09-05T00:00:00Z',
  last_failure: null,
  available_update: {
    version: '1.2.0',
    published_at: '2026-09-04T00:00:00Z',
    release_url: 'https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0',
    manifest_url: 'https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json',
    download_available: true,
    download_error: null,
  },
  skipped: false,
  download_supported: false,
}

class FakeApi implements UpdateApi {
  checks = 0
  status = structuredClone(available)

  async get(): Promise<UpdateStatus> {
    return structuredClone(this.status)
  }

  async check(): Promise<UpdateStatus> {
    this.checks += 1
    return structuredClone(this.status)
  }

  async skip(version: string | null): Promise<UpdateStatus> {
    this.status.skipped = version === this.status.available_update?.version
    return structuredClone(this.status)
  }
}

class FakeDesktop implements DesktopUpdateBridge {
  installs = 0

  async snapshot(): Promise<DesktopUpdateSnapshot> {
    return {
      phase: 'idle',
      target_version: null,
      downloaded_bytes: 0,
      total_bytes: null,
      error: null,
    }
  }

  async download(version: string): Promise<DesktopUpdateSnapshot> {
    return {
      phase: 'downloaded',
      target_version: version,
      downloaded_bytes: 42,
      total_bytes: 42,
      error: null,
    }
  }

  async install(): Promise<void> {
    this.installs += 1
  }

  async onProgress(): Promise<() => void> {
    return () => undefined
  }
}

describe('ProductUpdateCoordinator', () => {
  test('Windows leaves install progress to the native NSIS window', () => {
    expect(supportsInAppInstallProgress('Windows NT 10.0')).toBeFalse()
    expect(supportsInAppInstallProgress('X11; Linux x86_64')).toBeTrue()
  })

  test('automatic checks notify once per session and exact skip keeps Settings state', async () => {
    const api = new FakeApi()
    const coordinator = new ProductUpdateController(api)

    await coordinator.automaticCheck()
    expect(coordinator.notification?.version).toBe('1.2.0')
    coordinator.dismissNotification()
    await coordinator.automaticCheck()
    expect(api.checks).toBe(1)
    expect(coordinator.notification).toBeNull()

    await coordinator.skipAvailableVersion()
    expect(coordinator.status?.available_update?.version).toBe('1.2.0')
    expect(coordinator.status?.skipped).toBeTrue()
  })

  test('manual check reports up-to-date and failures while automatic failures stay quiet', async () => {
    const api = new FakeApi()
    api.status = { ...available, check_status: 'up-to-date', available_update: null }
    const coordinator = new ProductUpdateController(api)

    const upToDate = await coordinator.manualCheck()
    expect(upToDate).toBe('up-to-date')

    api.status = {
      ...api.status,
      check_status: 'error',
      last_failure: {
        code: 'UPDATE_REQUEST_FAILED',
        message: 'offline',
        attempted_at: '2026-09-05T00:00:00Z',
      },
    }
    expect(await coordinator.manualCheck()).toBe('error')
    coordinator.dismissNotification()
    await coordinator.automaticCheck()
    expect(coordinator.notification).toBeNull()
  })

  test('a newer discovery keeps the downloaded version installable after choosing later', async () => {
    const api = new FakeApi()
    api.status.download_supported = true
    const desktop = new FakeDesktop()
    const coordinator = new ProductUpdateController(api, desktop)
    await coordinator.load()

    await coordinator.downloadAvailableUpdate()
    expect(coordinator.state.targetVersion).toBe('1.2.0')
    expect(coordinator.state.installPromptOpen).toBeTrue()
    coordinator.dismissInstallPrompt()

    api.status.available_update = {
      ...api.status.available_update!,
      version: '1.3.0',
      release_url: 'https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.3.0',
    }
    await coordinator.manualCheck()
    expect(coordinator.state.targetVersion).toBe('1.2.0')
    expect(coordinator.state.downloadedReleaseUrl).toBe(available.available_update!.release_url)

    coordinator.requestInstallPrompt()
    expect(coordinator.state.installPromptOpen).toBeTrue()
    await coordinator.installDownloadedUpdate()
    expect(desktop.installs).toBe(1)
  })
})
