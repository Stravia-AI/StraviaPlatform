import { describe, expect, test } from 'bun:test'

import { ProductUpdateController } from '../src/lib/product-update'
import type { UpdateApi, UpdateStatus } from '../src/lib/product-update'

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

describe('ProductUpdateCoordinator', () => {
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
})
