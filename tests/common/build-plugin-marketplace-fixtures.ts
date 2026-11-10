import { createHash, createPrivateKey, createPublicKey, sign } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(fileURLToPath(new URL('../..', import.meta.url)))
const fixtures = resolve(root, 'target/vendor-test-fixtures')
const catalogName = 'stravia-plugin-catalog.json'
const timestamp = '2026-01-01T00:00:00Z'
const sha256 = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex')

// 公共测试种子只用于隔离 fixture；绝不能成为生产发布的信任根。
function signingKey(rotated: boolean) {
  const seed = createHash('sha256').update(`Stravia marketplace test-only key ${rotated ? 'rotated' : 'primary'}`).digest()
  const privateKey = createPrivateKey({
    key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed]),
    format: 'der',
    type: 'pkcs8',
  })
  const publicBytes = createPublicKey(privateKey).export({ format: 'der', type: 'spki' }).subarray(-32)
  const keyId = createHash('sha256').update(publicBytes).digest().subarray(0, 8)
  const publicPacket = Buffer.concat([Buffer.from('Ed'), keyId, publicBytes])
  const publicKey = `untrusted comment: Stravia isolated test public key\n${publicPacket.toString('base64')}\n`
  return { privateKey, keyId, publicKey }
}

async function build(version: number, directory: string, rotated = false) {
  const component = await readFile(resolve(fixtures, `lifecycle-v${version}.wasm`))
  const compressed = Bun.zstdCompressSync(component, { level: 19 })
  if (!Buffer.from(Bun.zstdDecompressSync(compressed)).equals(component)) throw new Error('Fixture compression round-trip failed')
  const filename = `fixture.lifecycle-v${version}.0.0.wasm.zst`
  const catalog = Buffer.from(JSON.stringify({
    schema_version: 2,
    repository_id: 424242,
    release_tag: `v${version}.0.0`,
    generated_at: timestamp,
    releases: [{
      vendor_id: 'fixture.lifecycle',
      version: `${version}.0.0`,
      display_name: 'Lifecycle Contract Fixture',
      description: 'Exercises generation pinning, private state, and network authorization',
      authors: ['Unverified fixture author'],
      published_at: timestamp,
      compatibility: { host_version_req: '>=0.6.0', wit_version: '0.6.0', canonical_format_version: 1 },
      artifact: { filename, compression: 'zstd', media_type: 'application/zstd', size: compressed.byteLength, sha256: sha256(compressed) },
      component: { size: component.byteLength, sha256: sha256(component) },
    }],
  }) + '\n')
  const { privateKey, keyId, publicKey } = signingKey(rotated)
  const digest = createHash('blake2b512').update(catalog).digest()
  const signature = sign(null, digest, privateKey)
  const packet = Buffer.concat([Buffer.from('ED'), keyId, signature])
  const comment = 'timestamp:1767225600\tfile:catalog\thashed'
  const globalSignature = sign(null, Buffer.concat([signature, Buffer.from(comment)]), privateKey)
  const minisig = `untrusted comment: Stravia isolated catalog signature\n${packet.toString('base64')}\ntrusted comment: ${comment}\n${globalSignature.toString('base64')}\n`
  await mkdir(directory, { recursive: true })
  await Promise.all([
    writeFile(resolve(directory, filename), compressed),
    writeFile(resolve(directory, catalogName), catalog),
    writeFile(resolve(directory, `${catalogName}.minisig`), minisig),
    writeFile(resolve(directory, 'stravia-plugin-catalog.pub'), publicKey),
  ])
}

await build(2, resolve(fixtures, 'marketplace'))
for (const version of [1, 2, 3]) await build(version, resolve(fixtures, `marketplace/v${version}`))
await build(2, resolve(fixtures, 'marketplace/rotated'), true)
console.log('Built signed isolated marketplace catalogs for lifecycle v1/v2/v3 and key rotation')
