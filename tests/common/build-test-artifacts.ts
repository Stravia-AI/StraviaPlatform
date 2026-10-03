import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs'
import { cpus } from 'node:os'
import { basename, join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '../..')
const suffix = process.platform === 'win32' ? '.exe' : ''
const checking = process.argv[2] === '--status'
const mode = process.argv[checking ? 3 : 2]
const directory = join(repository, 'target', 'test-artifacts', mode ?? '')
const manifestPath = join(directory, 'manifest.json')
const workspace = [
  '--locked',
  '--workspace',
  '--exclude',
  'stravia-desktop',
  '--exclude',
  'stravia-vendor-capability-contract-fixture',
  '--exclude',
  'stravia-vendor-lifecycle-contract-fixture',
  '--exclude',
  'stravia-vendor-management-contract-fixture',
  '--features',
  'stravia-server/test-harness',
]
interface Plan {
  command: string[]
  binaries?: string[]
  tests?: boolean
}
const plans: Record<string, Plan> = {
  workspace: { command: ['cargo', 'test', ...workspace, '--no-run', '--message-format=json'], tests: true },
  'desktop-tests': {
    command: ['cargo', 'test', '--locked', '-p', 'stravia-desktop', '--no-run', '--message-format=json'],
    tests: true,
  },
  services: {
    command: [
      'cargo',
      'build',
      '--locked',
      '--release',
      '-p',
      'stravia-server',
      '-p',
      'stravia-devtools',
      '--features',
      'stravia-server/test-harness',
    ],
    binaries: ['stravia-server', 'stravia-tools', 'stravia-storage-test-harness'].map((name) =>
      join(repository, 'target', 'release', `${name}${suffix}`),
    ),
  },
  release: {
    command: ['cargo', 'build', '--locked', '-p', 'stravia-server', '--release'],
    binaries: [join(repository, 'target', 'release', `stravia-server${suffix}`)],
  },
  development: {
    // dev:server 必须真正调用 cargo run；隔离其默认 feature graph，避免其它编译将它置脏。
    command: [
      'cargo',
      'build',
      '--locked',
      '-p',
      'stravia-server',
      '--target-dir',
      join(repository, 'target', 'test-development'),
    ],
    binaries: [join(repository, 'target', 'test-development', 'debug', `stravia-server${suffix}`)],
  },
  'desktop-native': {
    command: ['task', '--color=false', 'build:desktop:e2e'],
    binaries: [join(repository, 'target', 'debug', `stravia-desktop${suffix}`)],
  },
}
if (!mode || !Object.hasOwn(plans, mode))
  throw new Error('Expected workspace, desktop-tests, services, release, development or desktop-native')
const plan = plans[mode]
const cargoVersion = spawnSync('cargo', ['-vV'], { cwd: repository, encoding: 'utf8' })
if (cargoVersion.error) throw cargoVersion.error
if (cargoVersion.status !== 0) throw new Error('Cannot identify the locked Rust toolchain')
const rustVersion = spawnSync(process.env.RUSTC ?? 'rustc', ['-vV'], { cwd: repository, encoding: 'utf8' })
if (rustVersion.error) throw rustVersion.error
if (rustVersion.status !== 0) throw new Error('Cannot identify the Rust compiler')
const compilerKeys = [
  'RUSTC',
  'RUSTC_WRAPPER',
  'RUSTC_WORKSPACE_WRAPPER',
  'RUSTDOC',
  'RUSTUP_TOOLCHAIN',
  'RUSTFLAGS',
  'CARGO_ENCODED_RUSTFLAGS',
  'CARGO_TARGET_DIR',
  'TAURI_CONFIG',
  'SOURCE_DATE_EPOCH',
  'PATH',
  'INCLUDE',
  'LIB',
  'LIBPATH',
  'CC',
  'CXX',
  'AR',
  'CFLAGS',
  'CXXFLAGS',
]
const compilerEnvironment = Object.fromEntries(
  Object.entries({
    ...process.env,
    CARGO_TARGET_DIR: join(repository, 'target', mode === 'development' ? 'test-development' : ''),
    // Windows 下多个大型 Core rlib 同时 mmap 会耗尽 commit limit；限制编译峰值而非测试覆盖。
    CARGO_BUILD_JOBS: '4',
  })
    .filter(
      ([key]) =>
        compilerKeys.includes(key) ||
        /^(?:CARGO_(?:PROFILE|TARGET|BUILD)_|(?:CC|CXX|AR|CFLAGS|CXXFLAGS)_|STRAVIA_VENDOR_)/.test(key),
    )
    .sort(([a], [b]) => a.localeCompare(b)),
)
// 缓存中只记录摘要，不写出可能存在于编译环境中的私有值。
const signature = createHash('sha256')
  .update(
    JSON.stringify({
      cargo: cargoVersion.stdout,
      rustc: rustVersion.stdout,
      platform: process.platform,
      arch: process.arch,
      cpu: cpus()[0]?.model,
      command: plan.command,
      environment: compilerEnvironment,
    }),
  )
  .digest('hex')
interface Artifact {
  name: string
  executable: string
  size: number
  mtimeMs: number
}
interface Manifest {
  signature: string
  artifacts: Artifact[]
}
if (checking) {
  let current = false
  if (existsSync(manifestPath)) {
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as Manifest
    current =
      manifest.signature === signature &&
      manifest.artifacts.length > 0 &&
      manifest.artifacts.every((artifact) => {
        if (!existsSync(artifact.executable)) return false
        const file = statSync(artifact.executable)
        return file.isFile() && file.size === artifact.size && file.mtimeMs === artifact.mtimeMs
      })
  }
  process.exitCode = current ? 0 : 1
} else {
  mkdirSync(directory, { recursive: true })
  const result = spawnSync(plan.command[0], plan.command.slice(1), {
    cwd: repository,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, ...compilerEnvironment },
  })
  if (result.stderr) process.stderr.write(result.stderr)
  if (result.error) throw result.error
  if (result.status !== 0) {
    process.stdout.write(result.stdout ?? '')
    process.exitCode = result.status ?? 1
  } else {
    const artifacts: Artifact[] = []
    if (plan.tests) {
      const seen = new Set<string>()
      for (const line of result.stdout.split(/\r?\n/)) {
        if (!line.startsWith('{')) continue
        const artifact = JSON.parse(line)
        if (artifact.reason === 'compiler-message' && artifact.message.rendered)
          process.stderr.write(artifact.message.rendered)
        if (
          artifact.reason !== 'compiler-artifact' ||
          !artifact.profile.test ||
          !artifact.executable ||
          seen.has(artifact.executable)
        )
          continue
        seen.add(artifact.executable)
        const file = statSync(artifact.executable)
        artifacts.push({
          name: artifact.target.name,
          executable: artifact.executable,
          size: file.size,
          mtimeMs: file.mtimeMs,
        })
      }
    } else {
      process.stdout.write(result.stdout)
      for (const source of plan.binaries ?? []) {
        // Cargo 的根目录 exe 会被其它 feature graph 覆盖；测试只用本阶段的独立副本。
        const destination = join(directory, basename(source))
        copyFileSync(source, destination)
        const file = statSync(destination)
        artifacts.push({
          name: basename(source, suffix),
          executable: destination,
          size: file.size,
          mtimeMs: file.mtimeMs,
        })
      }
    }
    if (artifacts.length === 0) throw new Error(`${mode} produced no test artifacts`)
    const pending = `${manifestPath}.${process.pid}.pending`
    writeFileSync(pending, JSON.stringify({ signature, artifacts }, null, 2))
    renameSync(pending, manifestPath)
  }
}
