import { mkdir, copyFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const fixtures = dirname(fileURLToPath(import.meta.url))
const root = resolve(fixtures, '../../../..')
const target = resolve(root, 'target/vendor-fixture-build')
const output = resolve(root, 'target/vendor-test-fixtures')
const bundled = await Bun.file(resolve(root, 'target/vendor-plugins/manifest.json')).json() as Array<{
  vendor_id: string
  version: string
}>
const baseVersion = bundled.find((plugin) => plugin.vendor_id === 'base')?.version
if (!baseVersion) throw new Error('Build the bundled base component before the lifecycle fixtures')
const lifecycle = [
  ['lifecycle-v1', 'fixture.lifecycle', 'fixture.lifecycle', 'dedicated', '1.0.0', '1', '1', 'Unverified fixture author', 'fixture-lifecycle'],
  ['lifecycle-v2', 'fixture.lifecycle', 'fixture.lifecycle', 'dedicated', '2.0.0', '1', '1', 'Unverified fixture author', 'fixture-lifecycle'],
  ['lifecycle-v3', 'fixture.lifecycle', 'fixture.lifecycle', 'dedicated', '3.0.0', '2', '1', 'Unverified fixture author', 'fixture-lifecycle'],
  ['lifecycle-other-v1', 'fixture.lifecycle.other', 'fixture.lifecycle.other', 'dedicated', '1.0.0', '1', '1', 'Unverified fixture author', 'fixture-lifecycle'],
  ['lifecycle-host-incompatible', 'fixture.lifecycle', 'fixture.lifecycle', 'dedicated', '4.0.0', '2', '999', 'Unverified fixture author', 'fixture-lifecycle'],
  ['lifecycle-base-local', 'base', 'openai', 'fallback', '999.0.0', '1', '1', 'Stravia Builtin Team', 'openai-compatible'],
  ['lifecycle-base-older', 'base', 'openai', 'fallback', '0.0.0', '1', '1', 'Stravia Builtin Team', 'openai-compatible'],
  ['lifecycle-base-same', 'base', 'openai', 'fallback', baseVersion, '1', '1', 'Stravia Builtin Team', 'openai-compatible'],
  ['lifecycle-base-incompatible', 'base', 'openai', 'fallback', '0.0.0', '2', '1', 'Stravia Builtin Team', 'openai-compatible'],
] as const

async function buildFixture(directory: string, library: string, name: string, env: Record<string, string>) {
  const child = Bun.spawn([
    'cargo', 'build', '--locked', '--release', '--target', 'wasm32-wasip2',
    '--target-dir', target, '--manifest-path', resolve(fixtures, directory, 'Cargo.toml'),
  ], {
    cwd: root,
    env: { ...process.env, ...env },
    stdout: 'inherit',
    stderr: 'inherit',
  })
  if (await child.exited !== 0) throw new Error(`Failed to build ${name}`)
  await copyFile(resolve(target, `wasm32-wasip2/release/${library}.wasm`), resolve(output, `${name}.wasm`))
}

await mkdir(output, { recursive: true })
for (const [name, vendor, provider, kind, version, stateFormat, canonicalFormat, author, protocol] of lifecycle) {
  await buildFixture('lifecycle-contract', 'stravia_vendor_lifecycle_contract_fixture', name, {
    STRAVIA_FIXTURE_VENDOR_ID: vendor,
    STRAVIA_FIXTURE_PROVIDER_ID: provider,
    STRAVIA_FIXTURE_KIND: kind,
    STRAVIA_FIXTURE_VERSION: version,
    STRAVIA_FIXTURE_STATE_FORMAT: stateFormat,
    STRAVIA_FIXTURE_CANONICAL_FORMAT: canonicalFormat,
    STRAVIA_FIXTURE_AUTHOR: author,
    STRAVIA_FIXTURE_PROTOCOL: protocol,
    STRAVIA_FIXTURE_CREDENTIAL_KEY: 'apiKey',
  })
}
const capabilityProfiles = ['pure-search', 'pure-image', 'multi', 'removed', 'incompatible', 'base-older', 'dedicated-deepseek']
for (const profile of capabilityProfiles) {
  await buildFixture('capability-contract', 'stravia_vendor_capability_contract_fixture', `capability-${profile}`, {
    STRAVIA_CAPABILITY_PROFILE: profile,
  })
}
const managementProfiles = ['v1', 'v2', 'v3', 'other']
for (const profile of managementProfiles) {
  await buildFixture('management-contract', 'stravia_vendor_management_contract_fixture', `management-${profile}`, {
    STRAVIA_FIXTURE_PROFILE: profile,
  })
}
console.log(`Built ${lifecycle.length + capabilityProfiles.length + managementProfiles.length} real test components in ${output}`)
