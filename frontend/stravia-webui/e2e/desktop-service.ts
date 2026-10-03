import { execFile } from 'node:child_process'
import { rmSync } from 'node:fs'
import { join } from 'node:path'
import { promisify } from 'node:util'

import TauriWorkerService, { launcher as TauriLauncher } from '@wdio/tauri-service'

export default TauriWorkerService

// WDIO runs user onComplete hooks BEFORE launcher teardown. Directory cleanup
// belongs here, after the service has deleted sessions and stopped its drivers.
export class launcher extends TauriLauncher {
  override async onComplete(...args: Parameters<TauriLauncher['onComplete']>) {
    await super.onComplete(...args)
    const runRoot = process.env.STRAVIA_DESKTOP_E2E_RUN_ROOT
    if (!runRoot) return

    if (process.platform === 'win32') {
      // A WebView child can outlive the native host. Match the complete private
      // profile argument, never executable names/prefixes belonging to the user.
      const profile = join(runRoot, 'webview').replaceAll("'", "''")
      const script = `
$ErrorActionPreference = 'Stop'
$profile = '${profile}'
$owned = Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" | Where-Object {
  $argument = [regex]::Match($_.CommandLine, '(?:"--user-data-dir=([^"]+)"|--user-data-dir=(?:"([^"]+)"|([^ ]+)))')
  $directory = if ($argument.Groups[1].Success) { $argument.Groups[1].Value } elseif ($argument.Groups[2].Success) { $argument.Groups[2].Value } else { $argument.Groups[3].Value }
  $directory -eq $profile -or $directory -eq ($profile + '\\EBWebView')
}
foreach ($entry in $owned) {
  $child = Get-Process -Id $entry.ProcessId -ErrorAction SilentlyContinue
  if ($null -eq $child) { continue }
  try {
    $null = $child.Handle
    if ([Math]::Abs(($child.StartTime.ToUniversalTime() - $entry.CreationDate.ToUniversalTime()).TotalMilliseconds) -ge 1) {
      throw "Owned WebView PID changed before cleanup: $($entry.ProcessId)"
    }
    if (!$child.HasExited) { $child.Kill() }
    if (!$child.WaitForExit(5000)) { throw "Owned WebView process did not exit: $($entry.ProcessId)" }
  } finally { $child.Dispose() }
}
`
      await promisify(execFile)('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
        windowsHide: true,
      })
    }

    // An inherited root belongs to the caller, not this launcher.
    if (process.env.STRAVIA_DESKTOP_E2E_OWNS_RUN_ROOT !== '1') return
    for (let attempt = 1; ; attempt++) {
      try {
        rmSync(runRoot, { recursive: true, force: true })
        return
      } catch (error) {
        if (attempt >= 20) {
          throw new Error(`stravia-desktop-e2e: 无法清理临时目录 ${runRoot}`, { cause: error })
        }
        await new Promise<void>((resolve) => setTimeout(resolve, 250))
      }
    }
  }
}
