import { spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { createWriteStream, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { availableParallelism, tmpdir } from 'node:os'
import { basename, join, resolve } from 'node:path'
import { finished } from 'node:stream/promises'

const started = performance.now()
const repository = resolve(import.meta.dir, '../..')
const timestamp = new Date().toISOString().replace(/[:.]/g, '-')
const logDirectory = join(repository, 'target', 'test-results', 'all', timestamp)
const temporaryDirectory = mkdtempSync(join(tmpdir(), 'stravia-test-all-'))
mkdirSync(logDirectory, { recursive: true })
const cpuCount = availableParallelism()
const rustThreads = Math.max(1, Math.min(8, Math.floor(cpuCount / 4)))
const rustProcesses = Math.max(1, Math.min(3, Math.floor(cpuCount / 8)))
const pythonWorkers = Math.max(1, Math.min(6, Math.floor(cpuCount / 4)))
const browserWorkers = Math.max(1, Math.min(8, Math.floor(cpuCount / 4)))
const executableSuffix = process.platform === 'win32' ? '.exe' : ''
const environment: NodeJS.ProcessEnv = {
  ...process.env,
  TMP: temporaryDirectory,
  TEMP: temporaryDirectory,
  TMPDIR: temporaryDirectory,
  CI: '1',
  NO_COLOR: '1',
  FORCE_COLOR: '0',
  CARGO_TERM_COLOR: 'never',
  CARGO_BUILD_JOBS: '4',
  RAYON_NUM_THREADS: String(Math.max(1, Math.min(4, Math.floor(cpuCount / 8)))),
}
// 聚合入口只使用它自己创建的数据库，不继承可能指向非测试环境的连接。
delete environment.DB_URL
delete environment.DATABASE_URL
delete environment.STRAVIA_TEST_POSTGRES_URL
delete environment.STRAVIA_STORAGE_TEST_POSTGRES_URLS
delete environment.STRAVIA_STORAGE_HARNESS_BINARY
delete environment.STRAVIA_DESKTOP_E2E_RUN_ROOT

interface Result {
  name: string
  seconds: number
  exitCode: number
  log: string
}
interface RustBinary {
  name: string
  executable: string
}
interface RustManifest {
  name: string
  localIgnored: string[]
  externallyGated: string[]
}
const results: Result[] = []
const rustManifest: RustManifest[] = []
const preparationFailures: unknown[] = []

async function run(
  name: string,
  command: string[],
  options: { cwd?: string; env?: NodeJS.ProcessEnv; capture?: boolean } = {},
): Promise<string> {
  const begin = performance.now()
  const log = join(logDirectory, `${name.replace(/[^a-zA-Z0-9._-]/g, '_')}.log`)
  const output = createWriteStream(log)
  const child = spawn(command[0], command.slice(1), {
    cwd: options.cwd ?? repository,
    env: { ...environment, ...options.env },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let captured = ''
  child.stdout.setEncoding('utf8')
  child.stdout.on('data', (chunk: string) => {
    output.write(chunk)
    if (options.capture) captured += chunk
  })
  child.stderr.on('data', (chunk: Buffer) => output.write(chunk))
  let exitCode = 1
  try {
    exitCode = await new Promise<number>((resolveExit, reject) => {
      child.once('error', reject)
      child.once('close', (code) => resolveExit(code ?? 1))
    })
  } finally {
    output.end()
    await finished(output)
    const seconds = (performance.now() - begin) / 1000
    results.push({ name, seconds, exitCode, log })
    console.log(`[${exitCode === 0 ? 'PASS' : 'FAIL'}] ${name}: ${seconds.toFixed(2)}s`)
  }
  if (exitCode !== 0) throw new Error(`${name} exited ${exitCode}; see ${log}`)
  return captured
}

async function prepare(
  name: string,
  command: string[],
  inputsReady = true,
  options: { cwd?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<boolean> {
  if (!inputsReady) {
    console.log(`[BLOCKED] ${name}: prerequisite build failed`)
    return false
  }
  try {
    await run(name, command, options)
    return true
  } catch (error) {
    preparationFailures.push(error)
    return false
  }
}

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
const localIgnored: Record<string, true> = {
  'agent::artifact::tests::postgres_download_lifecycle_survives_reconstruction': true,
  'compaction::retention_tests::postgres_retention_contracts': true,
  'browser::tests::chrome_javascript_redirect_rebinds_readiness': true,
  'browser::tests::chrome_worlds_timeout_cancel_and_profile_cleanup': true,
  'browser::tests::chrome_google_challenges_stop_before_deadline': true,
  'browser::tests::chrome_dynamic_cookie_workers_frames_and_network_policy': true,
  paid_model_passes_through_without_injection: true,
  free_model_request_carries_full_contract: true,
  client_named_tool_overrides_placeholder: true,
  discovery_lists_only_free_models: true,
  authorization_code_exchange_fills_identity_from_bootstrap: true,
  inference_is_sent_in_claude_code_shape_and_tool_names_round_trip: true,
  oauth_and_inference_enforce_native_wire_at_real_http_boundary: true,
  no_code_continuation_error_requires_requested_id_before_response_created: true,
  affinity_and_account_headers_follow_actual_codex_frames: true,
  large_reasoning_signature_completes: true,
  real_base_executes_with_stable_wasi_patches_and_mixed_imports: true,
}

async function runRustBinaries(binaries: RustBinary[], requireLocalIgnored: boolean): Promise<void> {
  const failures: unknown[] = []
  let next = 0
  await Promise.all(
    Array.from({ length: rustProcesses }, async () => {
      while (next < binaries.length) {
        const binary = binaries[next++]
        const label = `rust-${basename(binary.executable, executableSuffix)}`
        try {
          await run(label, [binary.executable, `--test-threads=${rustThreads}`])
        } catch (error) {
          failures.push(error)
        }
        try {
          const listing = await run(`${label}-ignored-list`, [binary.executable, '--list', '--ignored'], {
            capture: true,
          })
          const ignored = listing
            .split(/\r?\n/)
            .filter((line) => line.endsWith(': test'))
            .map((line) => line.slice(0, -6))
          const selected = ignored.filter((name) => Object.hasOwn(localIgnored, name))
          rustManifest.push({
            name: binary.name,
            localIgnored: selected,
            externallyGated: ignored.filter((name) => !Object.hasOwn(localIgnored, name)),
          })
          if (selected.length > 0) {
            await run(`${label}-local-ignored`, [
              binary.executable,
              '--ignored',
              '--exact',
              `--test-threads=${rustThreads}`,
              ...selected,
            ])
          }
        } catch (error) {
          failures.push(error)
        }
      }
    }),
  )
  const selected = new Set(rustManifest.flatMap((entry) => entry.localIgnored))
  const missing = Object.keys(localIgnored).filter((name) => !selected.has(name))
  if (requireLocalIgnored && missing.length > 0)
    failures.push(new Error(`Missing local Rust scenarios: ${missing.join(', ')}`))
  if (failures.length > 0) throw new AggregateError(failures, 'Rust test failures')
}

const python = ['uv', 'run', '--locked', '--default-index', 'https://pypi.org/simple', '--group', 'test', 'python']
const composeFile = join(temporaryDirectory, 'postgres.compose.json')
const project = `stravia-test-${randomUUID().replaceAll('-', '').slice(0, 16)}`
const compose = ['docker', 'compose', '--project-name', project, '--file', composeFile]
let ownsPostgres = false
let failure: unknown

try {
  if (process.platform !== 'win32')
    throw new Error('The complete native desktop matrix currently requires Windows/WebView2')
  console.log(`Full matrix: ${logDirectory}`)
  console.log(`Workers: Rust ${rustProcesses} x ${rustThreads}, Python ${pythonWorkers}, Chromium ${browserWorkers}`)
  const password = randomUUID().replaceAll('-', '')
  writeFileSync(
    composeFile,
    JSON.stringify({
      services: {
        postgres: {
          image: 'postgres:16',
          pull_policy: 'never',
          environment: { POSTGRES_USER: 'stravia', POSTGRES_PASSWORD: password, POSTGRES_DB: 'stravia_test' },
          ports: ['127.0.0.1::5432'],
          healthcheck: {
            test: ['CMD', 'pg_isready', '-h', '127.0.0.1', '-U', 'stravia', '-d', 'stravia_test'],
            interval: '1s',
            timeout: '3s',
            retries: 30,
          },
        },
      },
    }),
    { mode: 0o600 },
  )
  ownsPostgres = true
  // Compose 自己等待真实数据库健康；镜像必须预先安装，不隐式下载或连接既有数据库。
  await run('postgres-start', [...compose, 'up', '--detach', '--wait', '--wait-timeout', '60'])
  const address = (await run('postgres-port', [...compose, 'port', 'postgres', '5432'], { capture: true })).trim()
  const port = /^127\.0\.0\.1:(\d+)$/.exec(address)?.[1]
  if (!port) throw new Error('Isolated PostgreSQL did not expose exactly one localhost port')
  const postgresUrl = `postgres://stravia:${password}@127.0.0.1:${port}/stravia_test`
  environment.DB_URL = postgresUrl
  environment.STRAVIA_TEST_POSTGRES_URL = postgresUrl
  // SQLx 的 session advisory 迁移锁按数据库隔离，随机 schema 不能隔离并行 worker。
  const storagePostgresUrls: Record<string, string> = {}
  const storageDatabaseCommands: string[] = []
  for (const worker of ['master', ...Array.from({ length: pythonWorkers }, (_, index) => `gw${index}`)]) {
    const database = `stravia_storage_${worker}`
    storageDatabaseCommands.push('-c', `CREATE DATABASE ${database} OWNER stravia`)
    storagePostgresUrls[worker] = `postgres://stravia:${password}@127.0.0.1:${port}/${database}`
  }
  // 只向本次新建的私有容器发无密码 SQL；每个 -c 独立提交 CREATE DATABASE。
  await run('postgres-storage-databases', [
    ...compose,
    'exec',
    '-T',
    'postgres',
    'psql',
    '-U',
    'stravia',
    '-d',
    'stravia_test',
    '-v',
    'ON_ERROR_STOP=1',
    ...storageDatabaseCommands,
  ])

  // 所有构建与 freshness 检查均在总计时内；先统一准备，再并行运行隔离的产品面。
  const pythonReady = await prepare('python-dependencies', ['uv', 'sync', '--locked', '--group', 'test'])
  const webReady = await prepare('production-web-assets', ['task', '--color=false', 'build:web'])
  const desktopWebReady = await prepare('desktop-web-assets', ['task', '--color=false', 'build:web:desktop-e2e'])
  const vendorsReady = await prepare('vendor-base', ['task', '--color=false', 'build:vendors'])
  const fixturesReady = await prepare(
    'vendor-fixtures',
    ['task', '--color=false', 'build:vendor-fixtures'],
    vendorsReady,
  )
  // Paraglide 会清空并重写生成目录；不能与 dev:server 的真实生成过程并发。
  await prepare('web-unit', ['bun', 'run', '--filter', 'stravia-webui', 'test:unit'])
  const workspaceReady = await prepare(
    'rust-build-tests',
    ['task', '--color=false', 'build:test:workspace'],
    webReady && fixturesReady,
  )
  const desktopTestsReady = await prepare(
    'desktop-unit-build',
    ['task', '--color=false', 'build:test:desktop-tests'],
    webReady && vendorsReady,
  )
  const binaries: RustBinary[] = []
  const seen = new Set<string>()
  for (const [mode, ready] of [
    ['workspace', workspaceReady],
    ['desktop-tests', desktopTestsReady],
  ] as const) {
    if (!ready) continue
    const manifest = await Bun.file(join(repository, 'target', 'test-artifacts', mode, 'manifest.json')).json()
    for (const artifact of manifest.artifacts) {
      if (seen.has(artifact.executable)) continue
      seen.add(artifact.executable)
      binaries.push({ name: artifact.name, executable: artifact.executable })
    }
  }
  const priority: Record<string, number> = {
    stravia_core: 0,
    vendor_plugin_lifecycle: 1,
    vendor_capability_contract: 2,
  }
  binaries.sort((a, b) => (priority[a.name] ?? 3) - (priority[b.name] ?? 3) || a.name.localeCompare(b.name))
  const servicesReady = await prepare(
    'release-server-and-harness',
    ['task', '--color=false', 'build:test:services'],
    webReady && vendorsReady,
  )
  const releaseReady = await prepare(
    'release-server',
    ['task', '--color=false', 'build:test:release'],
    webReady && vendorsReady,
  )
  const desktopReady = await prepare(
    'desktop-build',
    ['task', '--color=false', 'build:test:desktop-native'],
    desktopWebReady && vendorsReady,
  )
  const developmentReady = await prepare(
    'development-server-build',
    ['task', '--color=false', 'build:test:development'],
    webReady && vendorsReady,
  )

  const serviceServer = join(repository, 'target', 'test-artifacts', 'services', `stravia-server${executableSuffix}`)
  const releaseServer = join(repository, 'target', 'test-artifacts', 'release', `stravia-server${executableSuffix}`)
  environment.STRAVIA_STORAGE_HARNESS_BINARY = join(
    repository,
    'target',
    'test-artifacts',
    'services',
    `stravia-storage-test-harness${executableSuffix}`,
  )
  environment.STRAVIA_TOOLS_BINARY = join(
    repository,
    'target',
    'test-artifacts',
    'services',
    `stravia-tools${executableSuffix}`,
  )
  environment.STRAVIA_DESKTOP_E2E_BINARY = join(
    repository,
    'target',
    'test-artifacts',
    'desktop-native',
    `stravia-desktop${executableSuffix}`,
  )
  // 已编译的 Rust 测试不执行 Cargo/生成；可与隔离 feature graph 的真实 dev:server 并行。
  const rustOutcomes = Promise.allSettled(binaries.length > 0 ? [runRustBinaries(binaries, workspaceReady)] : [])
  // dev:server 完成后才启动 doctests 与 WebUI，避免 Cargo 与 Paraglide 生成过程争用。
  await prepare(
    'admin-development',
    [
      ...python,
      '-m',
      'pytest',
      'tests/e2e/admin/test_auth.py::test_development_task_accepts_vite_origin_without_bypassing_csrf',
      '-q',
    ],
    developmentReady && pythonReady,
    { env: { CARGO_TARGET_DIR: join(repository, 'target', 'test-development') } },
  )
  const remainingOutcomes = Promise.allSettled([
    ...(workspaceReady ? [run('rust-doctests', ['cargo', 'test', ...workspace, '--doc'])] : []),
    ...(desktopTestsReady
      ? [run('desktop-doctests', ['cargo', 'test', '--locked', '-p', 'stravia-desktop', '--doc'])]
      : []),
    ...(pythonReady && servicesReady
      ? [
          run('proxy', [...python, '-m', 'pytest', 'tests/e2e/proxy', '-q', '-m', 'proxy', '--durations=20'], {
            env: { STRAVIA_BINARY: serviceServer },
          }),
          run(
            'storage',
            [
              ...python,
              '-m',
              'pytest',
              'tests/e2e/storage',
              '-q',
              '-n',
              String(pythonWorkers),
              '--dist',
              'load',
              '--durations=20',
            ],
            {
              env: {
                STRAVIA_BINARY: serviceServer,
                STRAVIA_STORAGE_TEST_POSTGRES_URLS: JSON.stringify(storagePostgresUrls),
              },
            },
          ),
        ]
      : []),
    ...(pythonReady && releaseReady
      ? [
          run(
            'admin',
            [
              ...python,
              '-m',
              'pytest',
              'tests/e2e/admin',
              '-q',
              '-n',
              String(pythonWorkers),
              '--dist',
              'loadscope',
              '--durations=20',
              '--deselect',
              'tests/e2e/admin/test_auth.py::test_development_task_accepts_vite_origin_without_bypassing_csrf',
            ],
            { env: { STRAVIA_BINARY: releaseServer } },
          ),
        ]
      : []),
    ...(releaseReady
      ? [
          run(
            'web-chromium',
            [
              'bun',
              'run',
              '--filter',
              'stravia-webui',
              'test:e2e',
              '--workers',
              String(browserWorkers),
              '--retries',
              '0',
              '--reporter',
              'line',
            ],
            { env: { PLAYWRIGHT_USE_PREBUILT: '1', STRAVIA_BINARY: releaseServer } },
          ),
        ]
      : []),
    ...(desktopReady
      ? [
          (async () => {
            const failures: unknown[] = []
            for (const mode of ['', 'legacy', 'database']) {
              try {
                await run(mode ? `desktop-${mode}-recovery` : 'desktop', ['bun', 'run', 'test:e2e:desktop'], {
                  env: { STRAVIA_DESKTOP_E2E_STARTUP_FAILURE: mode },
                })
              } catch (error) {
                failures.push(error)
              }
            }
            if (failures.length > 0) throw new AggregateError(failures, 'Native desktop test failures')
          })(),
        ]
      : []),
    ...(pythonReady ? [run('updater-manifest', [...python, '.github/scripts/test_generate_updater_manifest.py'])] : []),
  ])
  const outcomes = [...(await rustOutcomes), ...(await remainingOutcomes)]
  const failures = [
    ...preparationFailures,
    ...outcomes
      .filter((result): result is PromiseRejectedResult => result.status === 'rejected')
      .map((result) => result.reason),
  ]
  if (failures.length > 0) throw new AggregateError(failures, 'Full matrix failed; inspect per-suite logs')
} catch (error) {
  failure = error
} finally {
  try {
    if (ownsPostgres)
      await run('postgres-cleanup', [...compose, 'down', '--volumes', '--remove-orphans', '--timeout', '10'])
  } catch (error) {
    failure = failure ? new AggregateError([failure, error], 'Tests and PostgreSQL cleanup failed') : error
  }
  try {
    rmSync(temporaryDirectory, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 })
  } catch (error) {
    failure = failure ? new AggregateError([failure, error], 'Tests and temporary-directory cleanup failed') : error
  }
  const seconds = (performance.now() - started) / 1000
  writeFileSync(
    join(logDirectory, 'summary.json'),
    JSON.stringify({ seconds, budgetSeconds: 300, passed: !failure && seconds <= 300, results, rustManifest }, null, 2),
  )
  console.log(`TOTAL ${seconds.toFixed(2)}s / 300s (builds, tests, cleanup included)`)
  if (seconds > 300)
    failure = failure
      ? new AggregateError([failure, new Error('Five-minute budget exceeded')])
      : new Error('Five-minute budget exceeded')
}
if (failure) {
  console.error(failure)
  process.exitCode = 1
}
