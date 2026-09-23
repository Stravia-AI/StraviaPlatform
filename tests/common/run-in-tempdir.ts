import { spawnSync } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const [command, ...args] = process.argv.slice(2)
if (!command) throw new Error('Usage: bun tests/common/run-in-tempdir.ts <command> [args...]')

const directory = mkdtempSync(join(tmpdir(), 'stravia-test-'))
let exitCode = 1
try {
  const result = spawnSync(command, args, {
    env: { ...process.env, TMP: directory, TEMP: directory, TMPDIR: directory },
    stdio: 'inherit',
  })
  if (result.error) throw result.error
  exitCode = result.status ?? 1
} finally {
  // 测试进程退出后，SQLite/WebView2 句柄不再阻止 Windows 删除整次运行的文件。
  rmSync(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 })
}
process.exitCode = exitCode
