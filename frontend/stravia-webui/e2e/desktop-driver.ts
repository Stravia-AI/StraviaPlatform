import { execFileSync } from 'node:child_process'
import { join } from 'node:path'

// tauri-service 1.4.0 has no persistent cache option. Provision through its supported
// PATH contract, then let the service independently check runtime compatibility.
// The Bun patch only teaches its version parser both official Microsoft banners;
// it does not bypass compatibility checks or replace the native executable.
export function prepareDesktopDriver(): void {
  if (process.platform !== 'win32') throw new Error('Native desktop smoke requires Windows and WebView2')
  const localAppData = process.env.LOCALAPPDATA
  if (!localAppData) throw new Error('LOCALAPPDATA is required for the per-user desktop driver cache')
  const cacheRoot = join(localAppData, 'Stravia', 'test-tools', 'msedgedriver')
  const script = String.raw`
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$root = $env:STRAVIA_DESKTOP_DRIVER_CACHE
$version = $env:EDGEDRIVER_VERSION
$pinned = -not [string]::IsNullOrEmpty($version)
if (!$pinned -and $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER) {
  $folder = $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER
  $runtime = Join-Path $folder 'msedgewebview2.exe'
  if (!(Test-Path -LiteralPath $runtime)) {
    $runtime = Get-ChildItem -LiteralPath $folder -Directory |
      Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
      Sort-Object { [version]$_.Name } -Descending |
      ForEach-Object { Join-Path $_.FullName 'msedgewebview2.exe' } |
      Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
  }
  if (!$runtime) { throw 'Fixed WebView2 runtime executable not found' }
  $version = (Get-Item -LiteralPath $runtime).VersionInfo.FileVersion.Split(' ')[0]
}
if (!$version) {
  foreach ($key in @(
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
    'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
    'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
  )) {
    if (Test-Path -LiteralPath $key) {
      $version = (Get-ItemProperty -LiteralPath $key).pv
      if ($version) { break }
    }
  }
}
if ($version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw 'Cannot determine a valid WebView2/driver version' }
# Microsoft requires matching major.minor.build, not just the major version.
$build = $version.Split('.')[0..2] -join '.'
function Compatible($path) {
  if (!(Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
  if ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Refusing a reparse-point driver'
  }
  $reported = (& $path --version) -join [Environment]::NewLine
  if ($LASTEXITCODE -ne 0 -or $reported -notmatch '(?:MSEdgeDriver|Microsoft Edge WebDriver) (\d+\.\d+\.\d+\.\d+)') { return $false }
  if ($pinned) { return $Matches[1] -eq $version }
  return ($Matches[1].Split('.')[0..2] -join '.') -eq $build
}
$existing = Get-Command msedgedriver.exe -CommandType Application -ErrorAction SilentlyContinue |
  Select-Object -First 1
# LOCALAPPDATA inherits the user's ACL; reject redirection out of that private cache.
$null = New-Item -ItemType Directory -Path $root -Force
$directory = Get-Item -LiteralPath $root
while ($directory) {
  if ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a redirected driver cache' }
  $directory = $directory.Parent
}
$slot = Join-Path $root $(if ($pinned) { $version } else { $build })
$driver = Join-Path $slot 'msedgedriver.exe'
if (Test-Path -LiteralPath $slot) {
  if ((Get-Item -LiteralPath $slot).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Refusing a redirected driver cache slot'
  }
  if (Compatible $driver) { [Console]::WriteLine($slot); exit 0 }
}
if ($existing -and (Compatible $existing.Source)) {
  $null = New-Item -ItemType Directory -Path $slot -Force
  Copy-Item -LiteralPath $existing.Source -Destination $driver -Force
  if (!(Compatible $driver)) { throw 'Installed Microsoft driver failed cache verification' }
  [Console]::WriteLine($slot)
  exit 0
}
$staging = Join-Path $root ('download-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $staging
try {
  $arch = if ([Environment]::Is64BitOperatingSystem) { 'win64' } else { 'win32' }
  $zip = Join-Path $staging 'driver.zip'
  Invoke-WebRequest -Uri "https://msedgedriver.microsoft.com/$version/edgedriver_$arch.zip" -OutFile $zip -UseBasicParsing -TimeoutSec 60
  Expand-Archive -LiteralPath $zip -DestinationPath $staging
  $downloaded = Join-Path $staging 'msedgedriver.exe'
  if (!(Compatible $downloaded)) { throw 'Downloaded Microsoft driver failed version verification' }
  $null = New-Item -ItemType Directory -Path $slot -Force
  Copy-Item -LiteralPath $downloaded -Destination $driver -Force
  if (!(Compatible $driver)) { throw 'Cached Microsoft driver failed version verification' }
  [Console]::WriteLine($slot)
} finally {
  Remove-Item -LiteralPath $staging -Recurse -Force
}
`
  const driverDirectory = execFileSync(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-EncodedCommand', Buffer.from(script, 'utf16le').toString('base64')],
    { encoding: 'utf8', timeout: 90_000, env: { ...process.env, STRAVIA_DESKTOP_DRIVER_CACHE: cacheRoot } },
  ).trim()
  if (!driverDirectory) throw new Error('Desktop driver preparation returned no driver directory')
  process.env.PATH = `${driverDirectory};${process.env.PATH ?? ''}`
}
