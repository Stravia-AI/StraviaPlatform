import { describe, expect, test } from 'bun:test'

import {
  ProviderConnectionDraftController,
  type ProviderConnectionDraftFields,
  type ProviderConnectionDraftTarget,
  type ProviderConnectionSubmission,
} from '../src/lib/provider-connection-draft'
import type {
  Provider,
  ProviderConfigurationPreview,
  ProviderConfigurationPreviewInput,
  ProviderDescriptor,
  VendorChannelDescriptor,
} from '../src/lib/types'

const channel: VendorChannelDescriptor = {
  id: 'default',
  name: { 'en-US': 'Default' },
  auth: { flow: 'authorization_code' },
  protocol: 'openai',
  capabilities: ['infer', 'config_validation'],
  search_model_required: false,
}

const descriptor: ProviderDescriptor = {
  provider_id: 'test-vendor',
  catalog_id: null,
  display_name: 'Test Vendor',
  description: null,
  channels: [channel],
  capabilities: [],
  config_fields: [
    {
      key: 'api_key',
      label: { 'en-US': 'API key' },
      kind: { type: 'string', multiline: false },
      required: true,
      secret: true,
    },
    {
      key: 'region',
      label: { 'en-US': 'Region' },
      kind: { type: 'string', multiline: false },
      required: false,
      secret: false,
    },
  ],
  config_groups: [],
  network: { extra_origins: [], field_origins: [] },
  data_compat: { config_fields_format: 0, private_state_format: 0, credentials_format: 0, model_metadata_format: 0 },
}

const existingProvider: Provider = {
  id: 'provider-1',
  name: 'Existing',
  vendor: 'test-vendor',
  channel: 'default',
  protocol: 'openai',
  base_url: 'https://existing.example.com',
  use_proxy: false,
  vendor_options: { region: 'us' },
  configured_credential_fields: ['api_key'],
  is_enabled: true,
  created_at: '',
  updated_at: '',
}

const savedProvider: Provider = { ...existingProvider, name: 'Saved Name', base_url: 'https://normalized.example.com' }

function createController(
  init: {
    edit?: boolean
    fields?: Partial<ProviderConnectionDraftFields>
    preview?: (input: ProviderConfigurationPreviewInput) => Promise<ProviderConfigurationPreview>
    write?: (submission: ProviderConnectionSubmission) => Promise<Provider>
    bind?: (provider: Provider, sessionId: string) => Promise<void>
  } = {},
) {
  const target: ProviderConnectionDraftTarget = init.edit
    ? { kind: 'edit', provider: existingProvider, descriptor, channel }
    : { kind: 'create', descriptor, channel }
  const savedSignal = Promise.withResolvers<Provider>()
  const calls = {
    previews: [] as ProviderConfigurationPreviewInput[],
    writes: [] as ProviderConnectionSubmission[],
    binds: [] as { provider: Provider; sessionId: string }[],
    cancels: 0,
    consumes: 0,
    proxyUpdates: [] as boolean[],
    saved: [] as Provider[],
    savedWhileSubmitting: [] as boolean[],
    partials: [] as { provider: Provider; error: unknown }[],
  }
  const controller = new ProviderConnectionDraftController({
    fields: {
      name: 'Draft Name',
      baseUrl: ' https://raw.example.com ',
      protocol: '',
      useProxy: false,
      values: { api_key: 'sk-test', region: 'us' },
      ...init.fields,
    },
    target: () => target,
    api: {
      previewConfiguration: (input) => {
        calls.previews.push(input)
        return init.preview
          ? init.preview(input)
          : Promise.resolve({ base_url: input.base_url, issues: [], network_permissions: [] })
      },
    },
    writer: {
      write: (submission) => {
        calls.writes.push(submission)
        return init.write ? init.write(submission) : Promise.resolve(savedProvider)
      },
      bind: init.bind
        ? (provider, sessionId) => {
            calls.binds.push({ provider, sessionId })
            return init.bind!(provider, sessionId)
          }
        : undefined,
    },
    oauth: {
      cancel: () => {
        calls.cancels += 1
        return Promise.resolve()
      },
      consume: () => {
        calls.consumes += 1
      },
      updateProxy: (useProxy) => {
        calls.proxyUpdates.push(useProxy)
        return Promise.resolve()
      },
    },
    hooks: {
      onSaved: (provider) => {
        calls.savedWhileSubmitting.push(controller.snapshot.submitting)
        calls.saved.push(provider)
        savedSignal.resolve(provider)
      },
      onPartialSave: (provider, error) => {
        calls.partials.push({ provider, error })
      },
    },
  })
  return { controller, calls, savedSignal }
}

describe('provider connection draft', () => {
  test('discards a stale preview response and never writes', async () => {
    const previewRequest = Promise.withResolvers<ProviderConfigurationPreview>()
    const { controller, calls } = createController({ preview: () => previewRequest.promise })

    const outcome = controller.submit()
    expect(calls.previews).toHaveLength(1)
    controller.configurationChanged()
    previewRequest.resolve({ base_url: 'https://new.example.com', issues: [], network_permissions: [] })

    expect(await outcome).toBe('aborted')
    expect(controller.snapshot.preview).toBeUndefined()
    expect(calls.writes).toHaveLength(0)
    expect(controller.snapshot.submitting).toBe(false)
  })

  test('ready auto-submits once across duplicate reports and concurrent submit', async () => {
    const writeRequest = Promise.withResolvers<Provider>()
    const writeInvoked = Promise.withResolvers<void>()
    const { controller, calls, savedSignal } = createController({
      write: () => {
        writeInvoked.resolve()
        return writeRequest.promise
      },
    })

    controller.oauthStateChanged('oauth-1', true)
    await writeInvoked.promise
    controller.oauthStateChanged('oauth-1', true)
    expect(await controller.submit()).toBe('ignored')
    writeRequest.resolve(savedProvider)
    await savedSignal.promise

    expect(calls.writes).toHaveLength(1)
    expect(calls.writes[0].sessionId).toBe('oauth-1')
    expect(calls.saved).toHaveLength(1)
    expect(calls.consumes).toBe(1)
    expect(controller.snapshot.submitting).toBe(false)
  })

  test('write succeeded but bind failed is partial success and keeps the raw draft', async () => {
    const bindRequest = Promise.withResolvers<void>()
    const bindInvoked = Promise.withResolvers<void>()
    const bindError = new Error('bind exploded')
    const { controller, calls } = createController({
      edit: true,
      fields: { baseUrl: ' https://edited.example.com ' },
      bind: () => {
        bindInvoked.resolve()
        return bindRequest.promise
      },
    })

    const submit = controller.submit()
    // ready 在提交期间到达：只记录 session，不触发第二次提交。
    controller.oauthStateChanged('oauth-7', true)
    await bindInvoked.promise
    expect(calls.binds).toHaveLength(1)
    expect(calls.binds[0].sessionId).toBe('oauth-7')
    bindRequest.reject(bindError)

    expect(await submit).toBe('partial')
    expect(calls.partials).toHaveLength(1)
    expect(calls.partials[0].provider.id).toBe('provider-1')
    expect(controller.snapshot.partialSaveError).toBe(bindError)
    expect(controller.snapshot.submitError).toBeUndefined()
    expect(controller.fields.baseUrl).toBe(' https://edited.example.com ')
    expect(controller.fields.name).toBe('Draft Name')
    expect(calls.consumes).toBe(0)
    expect(calls.saved).toHaveLength(0)
    expect(controller.snapshot.submitting).toBe(false)
  })

  test('submit lock covers preview through write and releases before onSaved', async () => {
    const writeRequest = Promise.withResolvers<Provider>()
    const writeInvoked = Promise.withResolvers<void>()
    const { controller, calls } = createController({
      write: () => {
        writeInvoked.resolve()
        return writeRequest.promise
      },
    })

    const submit = controller.submit()
    await writeInvoked.promise
    expect(controller.snapshot.submitting).toBe(true)
    expect(await controller.submit()).toBe('ignored')
    writeRequest.resolve(savedProvider)

    expect(await submit).toBe('saved')
    expect(calls.savedWhileSubmitting).toEqual([false])
    expect(controller.snapshot.submitting).toBe(false)
  })

  test('configuration change cancels oauth even before a session id exists', () => {
    const { controller, calls } = createController()

    controller.configurationChanged()

    expect(calls.cancels).toBe(1)
    expect(controller.snapshot.oauthSessionId).toBeUndefined()
  })

  test('edit save with a pending session skips bind and keeps the session alive', async () => {
    const { controller, calls } = createController({ edit: true, bind: () => Promise.resolve() })

    controller.oauthStateChanged('oauth-3', false)
    expect(await controller.submit()).toBe('saved')

    expect(calls.binds).toHaveLength(0)
    expect(calls.consumes).toBe(0)
    expect(controller.snapshot.oauthSessionId).toBe('oauth-3')
    expect(controller.fields.baseUrl).toBe('https://normalized.example.com')
    expect(controller.fields.name).toBe('Saved Name')
  })

  test('dispose ignores late oauth readiness and refuses further submit', async () => {
    const { controller, calls } = createController()

    controller.dispose()
    controller.oauthStateChanged('oauth-9', true)
    expect(await controller.submit()).toBe('ignored')

    expect(calls.writes).toHaveLength(0)
  })

  test('identical ready report after a failed submit does not auto-retry', async () => {
    const writeRequest = Promise.withResolvers<Provider>()
    const writeInvoked = Promise.withResolvers<void>()
    const { controller, calls } = createController({
      write: () => {
        writeInvoked.resolve()
        return writeRequest.promise
      },
    })

    const submit = controller.submit()
    // ready 在提交期间到达：记录最新 session 供 write/bind 使用。
    controller.oauthStateChanged('oauth-1', true)
    await writeInvoked.promise
    writeRequest.reject(new Error('write exploded'))
    expect(await submit).toBe('failed')
    expect(controller.snapshot.submitError).toBeInstanceOf(Error)

    // 相同的 ready 上报去重：一次 ready 只自动提交一次，失败不自动重试。
    controller.oauthStateChanged('oauth-1', true)
    expect(calls.writes).toHaveLength(1)
    expect(controller.snapshot.submitting).toBe(false)
  })

  test('dispose during an in-flight write aborts without firing onSaved', async () => {
    const writeRequest = Promise.withResolvers<Provider>()
    const writeInvoked = Promise.withResolvers<void>()
    const { controller, calls } = createController({
      write: () => {
        writeInvoked.resolve()
        return writeRequest.promise
      },
    })

    const submit = controller.submit()
    await writeInvoked.promise
    controller.dispose()
    writeRequest.resolve(savedProvider)

    expect(await submit).toBe('aborted')
    expect(calls.saved).toHaveLength(0)
    expect(calls.partials).toHaveLength(0)
  })

  test('preview issues stop before write and stay visible on the snapshot', async () => {
    const { controller, calls } = createController({
      preview: () =>
        Promise.resolve({
          base_url: 'https://raw.example.com',
          issues: [{ field: null, code: 'invalid', message: { 'en-US': 'bad config' } }],
          network_permissions: [],
        }),
    })

    expect(await controller.submit()).toBe('issues')
    expect(calls.writes).toHaveLength(0)
    expect(controller.snapshot.preview?.issues).toHaveLength(1)
    expect(controller.snapshot.submitting).toBe(false)
  })
})
