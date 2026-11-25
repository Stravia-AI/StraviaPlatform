import { rejects } from 'node:assert/strict'
import { describe, expect, test } from 'bun:test'

import { ProductUpdateController, isPrereleaseVersion, supportsInAppInstallProgress } from '../src/lib/product-update'
import type {
  DesktopUpdateBridge,
  DesktopUpdateSnapshot,
  UpdateApi,
  UpdateChannel,
  UpdateStatus,
} from '../src/lib/product-update'

const available: UpdateStatus = {
  current_version: '1.0.0',
  channel: 'release',
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
  channelChanges: UpdateChannel[] = []
  failChannelChange: Error | null = null
  nextCheckResolver: ((status: UpdateStatus) => void) | null = null
  deferNextCheck = false
  status = structuredClone(available)

  get(): Promise<UpdateStatus> {
    return Promise.resolve(structuredClone(this.status))
  }

  check(): Promise<UpdateStatus> {
    this.checks += 1
    if (this.deferNextCheck) {
      this.deferNextCheck = false
      const { promise, resolve } = Promise.withResolvers<UpdateStatus>()
      this.nextCheckResolver = resolve
      return promise
    }
    return Promise.resolve(structuredClone(this.status))
  }

  setChannel(channel: UpdateChannel): Promise<UpdateStatus> {
    this.channelChanges.push(channel)
    if (this.failChannelChange) {
      const error = this.failChannelChange
      this.failChannelChange = null
      return Promise.reject(error)
    }
    this.status = {
      ...this.status,
      channel,
      check_status: 'idle',
      last_success_at: null,
      last_failure: null,
      available_update: null,
      skipped: false,
    }
    return Promise.resolve(structuredClone(this.status))
  }

  skip(version: string | null): Promise<UpdateStatus> {
    this.status.skipped = version === this.status.available_update?.version
    return Promise.resolve(structuredClone(this.status))
  }
}

class FakeDesktop implements DesktopUpdateBridge {
  installs = 0

  snapshot(): Promise<DesktopUpdateSnapshot> {
    return Promise.resolve({ phase: 'idle', target_version: null, downloaded_bytes: 0, total_bytes: null, error: null })
  }

  download(version: string): Promise<DesktopUpdateSnapshot> {
    return Promise.resolve({
      phase: 'downloaded',
      target_version: version,
      downloaded_bytes: 42,
      total_bytes: 42,
      error: null,
    })
  }

  install(): Promise<void> {
    this.installs += 1
    return Promise.resolve()
  }

  onProgress(): Promise<() => void> {
    return Promise.resolve(() => undefined)
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
      last_failure: { code: 'UPDATE_REQUEST_FAILED', message: 'offline', attempted_at: '2026-09-05T00:00:00Z' },
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

  test('detects prerelease versions without confusing build metadata', () => {
    expect(isPrereleaseVersion('0.8.0-beta.1')).toBeTrue()
    expect(isPrereleaseVersion('0.8.0-beta.1+build.4')).toBeTrue()
    expect(isPrereleaseVersion('0.8.0')).toBeFalse()
    expect(isPrereleaseVersion('0.8.0+build-4')).toBeFalse()
  })

  test('switching from beta to release clears downloaded prerelease state and drops stale in-flight checks', async () => {
    const api = new FakeApi()
    api.status = {
      ...structuredClone(available),
      current_version: '0.8.0-beta.1',
      channel: 'beta',
      download_supported: true,
      available_update: { ...structuredClone(available.available_update!), version: '0.8.0-beta.2' },
    }
    const desktop = new FakeDesktop()
    const coordinator = new ProductUpdateController(api, desktop)
    await coordinator.load()

    await coordinator.downloadAvailableUpdate()
    expect(coordinator.state.phase).toBe('downloaded')
    expect(coordinator.state.targetVersion).toBe('0.8.0-beta.2')

    const switched = await coordinator.setChannel('release')
    expect(switched.channel).toBe('release')
    expect(coordinator.state.phase).toBe('idle')
    expect(coordinator.state.targetVersion).toBeNull()
    expect(coordinator.state.installPromptOpen).toBeFalse()

    // 旧检查在切换通道后才返回时，不得覆盖新通道状态。
    api.deferNextCheck = true
    const staleCheck = coordinator.silentCheck()
    const staleSnapshot = structuredClone(api.status)
    await coordinator.setChannel('beta')
    api.nextCheckResolver?.({ ...staleSnapshot, channel: 'release', check_status: 'up-to-date' })
    await staleCheck
    expect(coordinator.status?.channel).toBe('beta')
    expect(coordinator.status?.check_status).toBe('idle')
  })

  test('channel save failure preserves server-confirmed channel', async () => {
    const api = new FakeApi()
    const coordinator = new ProductUpdateController(api)
    await coordinator.load()

    api.failChannelChange = new Error('storage unavailable')
    await rejects(() => coordinator.setChannel('beta'), /storage unavailable/)
    expect(coordinator.status?.channel).toBe('release')
  })
})
