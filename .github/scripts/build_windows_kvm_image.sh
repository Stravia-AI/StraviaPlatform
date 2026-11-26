#!/usr/bin/env bash
# Build a minimal, lockfile-independent, read-only Windows 10 Enterprise LTSC 2021 Eval
# golden qcow2 image (24G disk + offline WebView2 Standalone + Python 3.12 + OpenSSH with
# runtime STRAVIA_SEED key sync + secondary trimming + CompactOS + zeroing + zstd qcow2)
# for Layer-2 KVM Windows WebView2 Desktop E2E on Linux x64 hosts.
#
# Note: Project dependency caches (bun-cache / uv-cache) are intentionally NOT baked into
# the OS base image so lockfile updates never invalidate the golden OS image. They are
# cached externally on the host by `.github/scripts/kvm_windows_e2e.sh`.
#
# Usage (standalone):
#   bash .github/scripts/build_windows_kvm_image.sh
#
# Usage (offline / Nix sandbox, all asset paths pre-supplied):
#   OFFLINE_BUILD=1 SKIP_ISO_SHA256=1 \
#   WIN10_LTSC_ISO_PATH=... VIRTIO_ISO_PATH=... WEBVIEW2_INSTALLER_PATH=... \
#   OPENSSH_ZIP_PATH=... NODE_ZIP_PATH=... BUN_ZIP_PATH=... UV_ZIP_PATH=... \
#   PYTHON_STANDALONE_TAR_PATH=... OUTPUT_IMAGE=... METADATA_OUTPUT=... \
#   bash .github/scripts/build_windows_kvm_image.sh
set -euo pipefail

VM_HOME="${VM_HOME:-${HOME:-/tmp}/.cache/stravia-win-vm}"
ASSETS_DIR="${ASSETS_DIR:-${VM_HOME}/assets}"
OUTPUT_IMAGE="${OUTPUT_IMAGE:-${VM_HOME}/win10-ltsc-2021-e2e-golden.qcow2}"
METADATA_OUTPUT="${METADATA_OUTPUT:-}"
WINDOWS_SSH_USER="${WINDOWS_SSH_USER:-stravia}"
WINDOWS_PASSWORD="${WINDOWS_PASSWORD:-StraviaE2e!2026}"
BUILD_RAM="${BUILD_RAM:-8G}"
BUILD_CPUS="${BUILD_CPUS:-4}"
DISK_SIZE="${DISK_SIZE:-24G}"
BUILD_TIMEOUT="${BUILD_TIMEOUT:-2700}"
OFFLINE_BUILD="${OFFLINE_BUILD:-0}"
SKIP_ISO_SHA256="${SKIP_ISO_SHA256:-0}"

# Pinned immutable upstream URLs (mirrored in nix/win-vm/assets.nix)
WIN10_LTSC_ISO_URL="https://software-download.microsoft.com/download/pr/19044.1288.211006-0501.21h2_release_svc_refresh_CLIENT_LTSC_EVAL_x64FRE_en-us.iso"
WIN10_LTSC_ISO_SHA256="e4ab2e3535be5748252a8d5d57539a6e59be8d6726345ee10e7afd2cb89fefb5"
VIRTIO_VERSION="${VIRTIO_VERSION:-0.1.302-1}"
VIRTIO_ISO_URL="https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-${VIRTIO_VERSION}/virtio-win.iso"
WEBVIEW2_STANDALONE_URL="https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/58f85fae-b5f5-4443-9b8d-6d4cf627d6f6/MicrosoftEdgeWebView2RuntimeInstallerX64.exe"
BUN_VERSION="${BUN_VERSION:-1.4.2}"
BUN_WIN_URL="https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-windows-x64.zip"
NODE_VERSION="${NODE_VERSION:-22.14.0}"
NODE_WIN_URL="https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-win-x64.zip"
# Pin Win32-OpenSSH v9.5.0.0p1-Beta: 10.x-Preview split sshd-session.exe fails to serve
# connections cleanly on Win10 LTSC 2021 (Build 19044).
OPENSSH_VERSION="${OPENSSH_VERSION:-v9.5.0.0p1-Beta}"
OPENSSH_WIN_URL="https://github.com/PowerShell/Win32-OpenSSH/releases/download/${OPENSSH_VERSION}/OpenSSH-Win64.zip"
UV_VERSION="${UV_VERSION:-0.13.10}"
UV_WIN_URL="https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-x86_64-pc-windows-msvc.zip"
PYTHON_VERSION="${PYTHON_VERSION:-3.12.16}"
PYTHON_BUILD_TAG="${PYTHON_BUILD_TAG:-20261124}"
PYTHON_STANDALONE_URL="https://github.com/astral-sh/python-build-standalone/releases/download/${PYTHON_BUILD_TAG}/cpython-${PYTHON_VERSION}%2B${PYTHON_BUILD_TAG}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  echo "ERROR: /dev/kvm is not accessible" >&2
  exit 1
fi

QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
QEMU_IMG_BIN="${QEMU_IMG_BIN:-$(command -v qemu-img || true)}"
SEVENZ_BIN="${SEVENZ_BIN:-$(command -v 7zz || command -v 7z || true)}"
MKISOFS_BIN="${MKISOFS_BIN:-$(command -v genisoimage || command -v mkisofs || command -v xorriso || true)}"
PYTHON3_BIN="${PYTHON3_BIN:-$(command -v python3 || true)}"

for pair in \
  "qemu-system-x86_64:${QEMU_BIN}" \
  "qemu-img:${QEMU_IMG_BIN}" \
  "7zz/7z:${SEVENZ_BIN}" \
  "genisoimage/mkisofs/xorriso:${MKISOFS_BIN}" \
  "python3:${PYTHON3_BIN}"; do
  name="${pair%%:*}"
  bin="${pair#*:}"
  if [[ -z "${bin}" || ! -x "${bin}" ]]; then
    echo "ERROR: Required host tool not found: ${name}" >&2
    exit 1
  fi
done

mkdir -p "$(dirname "${OUTPUT_IMAGE}")"
if [[ -n "${METADATA_OUTPUT}" ]]; then
  mkdir -p "$(dirname "${METADATA_OUTPUT}")"
fi
if [[ "${OFFLINE_BUILD}" != "1" ]]; then
  mkdir -p "${VM_HOME}" "${ASSETS_DIR}"
fi

download_if_missing() {
  local url="$1"
  local dest="$2"
  if [[ -f "${dest}" ]]; then
    return 0
  fi
  if [[ "${OFFLINE_BUILD}" == "1" ]]; then
    echo "ERROR: OFFLINE_BUILD=1 but required asset is missing: ${dest}" >&2
    exit 1
  fi
  echo "==> Downloading $(basename "${dest}")..."
  curl -fL --retry 3 --retry-delay 3 -C - -o "${dest}.part" "${url}"
  mv "${dest}.part" "${dest}"
}

WIN10_LTSC_ISO_PATH="${WIN10_LTSC_ISO_PATH:-${ASSETS_DIR}/win10-ltsc-2021-eval-x64.iso}"
VIRTIO_ISO_PATH="${VIRTIO_ISO_PATH:-${ASSETS_DIR}/virtio-win.iso}"
WEBVIEW2_INSTALLER_PATH="${WEBVIEW2_INSTALLER_PATH:-${ASSETS_DIR}/MicrosoftEdgeWebView2RuntimeInstallerX64.exe}"
OPENSSH_ZIP_PATH="${OPENSSH_ZIP_PATH:-${ASSETS_DIR}/OpenSSH-Win64-${OPENSSH_VERSION}.zip}"
NODE_ZIP_PATH="${NODE_ZIP_PATH:-${ASSETS_DIR}/node-v${NODE_VERSION}-win-x64.zip}"
BUN_ZIP_PATH="${BUN_ZIP_PATH:-${ASSETS_DIR}/bun-windows-x64.zip}"
UV_ZIP_PATH="${UV_ZIP_PATH:-${ASSETS_DIR}/uv-x86_64-pc-windows-msvc.zip}"
PYTHON_STANDALONE_TAR_PATH="${PYTHON_STANDALONE_TAR_PATH:-${ASSETS_DIR}/cpython-${PYTHON_VERSION}-windows-x64.tar.gz}"

download_if_missing "${WEBVIEW2_STANDALONE_URL}" "${WEBVIEW2_INSTALLER_PATH}"
download_if_missing "${BUN_WIN_URL}" "${BUN_ZIP_PATH}"
download_if_missing "${NODE_WIN_URL}" "${NODE_ZIP_PATH}"
download_if_missing "${OPENSSH_WIN_URL}" "${OPENSSH_ZIP_PATH}"
download_if_missing "${UV_WIN_URL}" "${UV_ZIP_PATH}"
download_if_missing "${PYTHON_STANDALONE_URL}" "${PYTHON_STANDALONE_TAR_PATH}"
download_if_missing "${VIRTIO_ISO_URL}" "${VIRTIO_ISO_PATH}"
download_if_missing "${WIN10_LTSC_ISO_URL}" "${WIN10_LTSC_ISO_PATH}"

if [[ "${SKIP_ISO_SHA256}" != "1" ]]; then
  echo "==> Verifying Win10 Enterprise LTSC 2021 Eval ISO SHA-256..."
  ACTUAL_SHA="$(sha256sum "${WIN10_LTSC_ISO_PATH}" | awk '{print $1}')"
  if [[ "${ACTUAL_SHA}" != "${WIN10_LTSC_ISO_SHA256}" ]]; then
    echo "ERROR: Win10 ISO SHA-256 mismatch: expected ${WIN10_LTSC_ISO_SHA256}, got ${ACTUAL_SHA}" >&2
    exit 1
  fi
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/stravia-win-build.XXXXXX")"
QEMU_PID=""
cleanup() {
  set +e
  if [[ -n "${QEMU_PID}" ]] && kill -0 "${QEMU_PID}" 2>/dev/null; then
    kill "${QEMU_PID}" 2>/dev/null || true
    wait "${QEMU_PID}" 2>/dev/null || true
  fi
  if [[ "${KEEP_BUILD_WORKDIR:-0}" != "1" ]]; then
    rm -rf "${WORK_DIR}" 2>/dev/null || true
  else
    echo "==> Kept build workdir: ${WORK_DIR}"
  fi
}
trap cleanup EXIT INT TERM

# Ephemeral build-only SSH keypair used purely for host->guest auth verification during build.
# Its public key is wiped from the golden image right before free-space zeroing; runtime SSH keys
# are injected dynamically per run via the STRAVIA_SEED ISO.
BUILD_SSH_KEY="${WORK_DIR}/build_id_ed25519"
ssh-keygen -t ed25519 -N "" -C "stravia-win-build-ephemeral" -f "${BUILD_SSH_KEY}" >/dev/null
chmod 0600 "${BUILD_SSH_KEY}"
BUILD_SSH_PUB="$(cat "${BUILD_SSH_KEY}.pub")"

PROV_DIR="${WORK_DIR}/provision"
mkdir -p "${PROV_DIR}/drivers" "${PROV_DIR}/tools"

echo "==> Extracting minimal VirtIO w10/amd64 drivers (viostor, NetKVM, vioserial)..."
for drv in viostor NetKVM vioserial; do
  mkdir -p "${PROV_DIR}/drivers/${drv}"
  "${SEVENZ_BIN}" x -y "-o${PROV_DIR}/drivers/${drv}" "${VIRTIO_ISO_PATH}" "${drv}/w10/amd64/*" >/dev/null || true
  if [[ -d "${PROV_DIR}/drivers/${drv}/${drv}/w10/amd64" ]]; then
    mv "${PROV_DIR}/drivers/${drv}/${drv}/w10/amd64/"* "${PROV_DIR}/drivers/${drv}/"
    rm -rf "${PROV_DIR}/drivers/${drv:?}/${drv}"
  fi
done
find "${PROV_DIR}/drivers" -name '*.pdb' -delete

echo "==> Staging offline Windows toolchains into provision ISO..."
cp "${WEBVIEW2_INSTALLER_PATH}" "${PROV_DIR}/tools/MicrosoftEdgeWebView2RuntimeInstallerX64.exe"
cp "${BUN_ZIP_PATH}" "${PROV_DIR}/tools/bun.zip"
cp "${NODE_ZIP_PATH}" "${PROV_DIR}/tools/node.zip"
cp "${OPENSSH_ZIP_PATH}" "${PROV_DIR}/tools/openssh.zip"
cp "${UV_ZIP_PATH}" "${PROV_DIR}/tools/uv.zip"
cp "${PYTHON_STANDALONE_TAR_PATH}" "${PROV_DIR}/tools/python312.tar.gz"
printf '%s\n' "${BUILD_SSH_PUB}" > "${PROV_DIR}/authorized_keys"

cat > "${PROV_DIR}/Autounattend.xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend"
          xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <SetupUILanguage>
        <UILanguage>en-US</UILanguage>
      </SetupUILanguage>
      <InputLocale>en-US</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>

    <component name="Microsoft-Windows-PnpCustomizationsWinPE"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <DriverPaths>
        <PathAndCredentials wcm:action="add" wcm:keyValue="1"><Path>D:\drivers\viostor</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="2"><Path>E:\drivers\viostor</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="3"><Path>F:\drivers\viostor</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="4"><Path>D:\drivers\NetKVM</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="5"><Path>E:\drivers\NetKVM</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="6"><Path>F:\drivers\NetKVM</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="7"><Path>D:\drivers\vioserial</Path></PathAndCredentials>
        <PathAndCredentials wcm:action="add" wcm:keyValue="8"><Path>E:\drivers\vioserial</Path></PathAndCredentials>
      </DriverPaths>
    </component>

    <component name="Microsoft-Windows-Setup"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <DiskConfiguration>
        <Disk wcm:action="add">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add">
              <Order>1</Order>
              <Size>100</Size>
              <Type>Primary</Type>
            </CreatePartition>
            <CreatePartition wcm:action="add">
              <Order>2</Order>
              <Extend>true</Extend>
              <Type>Primary</Type>
            </CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add">
              <Order>1</Order>
              <PartitionID>1</PartitionID>
              <Active>true</Active>
              <Format>NTFS</Format>
              <Label>System</Label>
            </ModifyPartition>
            <ModifyPartition wcm:action="add">
              <Order>2</Order>
              <PartitionID>2</PartitionID>
              <Format>NTFS</Format>
              <Label>Windows</Label>
              <Letter>C</Letter>
            </ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallFrom>
            <MetaData wcm:action="add">
              <Key>/IMAGE/INDEX</Key>
              <Value>1</Value>
            </MetaData>
          </InstallFrom>
          <InstallTo>
            <DiskID>0</DiskID>
            <PartitionID>2</PartitionID>
          </InstallTo>
        </OSImage>
      </ImageInstall>
      <UserData>
        <AcceptEula>true</AcceptEula>
        <FullName>Stravia E2E</FullName>
        <Organization>Stravia</Organization>
      </UserData>
    </component>
  </settings>

  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <ComputerName>STRAVIA-WIN-E2E</ComputerName>
      <TimeZone>UTC</TimeZone>
      <Display>
        <ColorDepth>32</ColorDepth>
        <HorizontalResolution>1920</HorizontalResolution>
        <VerticalResolution>1080</VerticalResolution>
        <RefreshRate>60</RefreshRate>
      </Display>
    </component>
    <component name="Microsoft-Windows-TerminalServices-LocalSessionManager"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <fDenyTSConnections>true</fDenyTSConnections>
    </component>
  </settings>

  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <InputLocale>en-US</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>

    <component name="Microsoft-Windows-Shell-Setup"
               processorArchitecture="amd64"
               publicKeyToken="31bf3856ad364e35"
               language="neutral"
               versionScope="nonSxS">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <NetworkLocation>Work</NetworkLocation>
        <ProtectYourPC>3</ProtectYourPC>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>${WINDOWS_SSH_USER}</Name>
            <DisplayName>${WINDOWS_SSH_USER}</DisplayName>
            <Group>Administrators</Group>
            <Password>
              <Value>${WINDOWS_PASSWORD}</Value>
              <PlainText>true</PlainText>
            </Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Username>${WINDOWS_SSH_USER}</Username>
        <Enabled>true</Enabled>
        <LogonCount>999999</LogonCount>
        <Password>
          <Value>${WINDOWS_PASSWORD}</Value>
          <PlainText>true</PlainText>
        </Password>
      </AutoLogon>
      <Display>
        <ColorDepth>32</ColorDepth>
        <HorizontalResolution>1920</HorizontalResolution>
        <VerticalResolution>1080</VerticalResolution>
        <RefreshRate>60</RefreshRate>
      </Display>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Description>Provision and trim Stravia E2E image</Description>
          <CommandLine>cmd.exe /c "for %d in (D E F G) do @if exist %d:\provision.ps1 powershell.exe -NoProfile -ExecutionPolicy Bypass -File %d:\provision.ps1 -MediaRoot %d:\ -Username ${WINDOWS_SSH_USER} -Password ${WINDOWS_PASSWORD}"</CommandLine>
          <RequiresUserInput>false</RequiresUserInput>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
EOF

cat > "${PROV_DIR}/provision.ps1" <<'PSEOF'
param(
  [Parameter(Mandatory = $true)][string]$MediaRoot,
  [Parameter(Mandatory = $true)][string]$Username,
  [Parameter(Mandatory = $true)][string]$Password
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$LogFile = 'C:\Windows\Temp\stravia-provision.log'

$script:ComPort = $null
try {
  $script:ComPort = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
  $script:ComPort.Open()
} catch {
  $script:ComPort = $null
}

function Write-Log([string]$Message) {
  $line = "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $Message"
  Write-Host $line
  Add-Content -Path $LogFile -Value $line -Encoding UTF8
  if ($script:ComPort -and $script:ComPort.IsOpen) {
    try { $script:ComPort.WriteLine($line) } catch {}
  }
}

try {
  Write-Log "=== Stravia Win10 LTSC 2021 Eval offline provisioning started (MediaRoot=$MediaRoot) ==="
  $freeBefore = [math]::Round((Get-PSDrive C).Used / 1GB, 2)
  Write-Log "Disk C: used before provisioning = ${freeBefore} GiB"

  # 1. Persistent AutoLogon, UAC off, screen/power never sleep, network prompt off
  Write-Log "Configuring AutoLogon, UAC, power settings, and 1920x1080 display..."
  $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
  Set-ItemProperty -Path $winlogon -Name 'AutoAdminLogon' -Value '1' -Type String
  Set-ItemProperty -Path $winlogon -Name 'DefaultUserName' -Value $Username -Type String
  Set-ItemProperty -Path $winlogon -Name 'DefaultPassword' -Value $Password -Type String
  Set-ItemProperty -Path $winlogon -Name 'DefaultDomainName' -Value $env:COMPUTERNAME -Type String
  Remove-ItemProperty -Path $winlogon -Name 'AutoLogonCount' -ErrorAction SilentlyContinue

  Set-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name 'EnableLUA' -Value 0 -Type DWord
  Set-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name 'ConsentPromptBehaviorAdmin' -Value 0 -Type DWord
  New-Item -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Network\NewNetworkWindowOff' -Force | Out-Null
  New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Force | Out-Null
  Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name 'NoLockScreen' -Value 1 -Type DWord
  Set-ItemProperty -Path 'HKCU:\Control Panel\Desktop' -Name 'ScreenSaveActive' -Value '0' -Type String

  & powercfg /h off | Out-Null
  & powercfg /change monitor-timeout-ac 0 | Out-Null
  & powercfg /change standby-timeout-ac 0 | Out-Null
  & powercfg /change disk-timeout-ac 0 | Out-Null

  # Cap pagefile to 512-1024 MB and zero it on shutdown so an 8 GiB auto-sized pagefile
  # neither bloats the 24G disk nor leaks non-zero blocks into the zstd golden image.
  try {
    $cs = Get-CimInstance Win32_ComputerSystem
    if ($cs -and $cs.AutomaticManagedPagefile) {
      $cs | Set-CimInstance -Property @{ AutomaticManagedPagefile = $false } | Out-Null
    }
    Get-CimInstance Win32_PageFileSetting -ErrorAction SilentlyContinue | Remove-CimInstance -ErrorAction SilentlyContinue
    New-CimInstance -ClassName Win32_PageFileSetting -Property @{
      Name = 'C:\pagefile.sys'
      InitialSize = 512
      MaximumSize = 1024
    } -ErrorAction SilentlyContinue | Out-Null
    Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management' -Name 'ClearPageFileAtShutdown' -Value 1 -Type DWord -ErrorAction SilentlyContinue
  } catch {}

  Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class DisplayHelper {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  public struct DEVMODE {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
    public short dmSpecVersion; public short dmDriverVersion; public short dmSize; public short dmDriverExtra;
    public int dmFields; public int dmPositionX; public int dmPositionY; public int dmDisplayOrientation;
    public int dmDisplayFixedOutput; public short dmColor; public short dmDuplex; public short dmYResolution;
    public short dmTTOption; public short dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
    public short dmLogPixels; public int dmBitsPerPel; public int dmPelsWidth; public int dmPelsHeight;
    public int dmDisplayFlags; public int dmDisplayFrequency;
    public int dmICMMethod; public int dmICMIntent; public int dmMediaType; public int dmDitherType;
    public int dmReserved1; public int dmReserved2; public int dmPanningWidth; public int dmPanningHeight;
  }
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern bool EnumDisplaySettings(string lpszDeviceName, int iModeNum, ref DEVMODE lpDevMode);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern int ChangeDisplaySettings(ref DEVMODE lpDevMode, int dwFlags);
  public static string Set1080p() {
    DEVMODE dm = new DEVMODE();
    dm.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
    if (!EnumDisplaySettings(null, -1, ref dm)) return "EnumFailed";
    dm.dmPelsWidth = 1920; dm.dmPelsHeight = 1080; dm.dmBitsPerPel = 32;
    dm.dmFields = 0x180000 | 0x40000;
    int res = ChangeDisplaySettings(ref dm, 1);
    return "res=" + res + " current=" + dm.dmPelsWidth + "x" + dm.dmPelsHeight;
  }
}
"@
  Write-Log ("Display resolution switch: " + [DisplayHelper]::Set1080p())

  # 2. Install remaining VirtIO drivers if any
  $drvRoot = Join-Path $MediaRoot 'drivers'
  if (Test-Path $drvRoot) {
    Write-Log "Installing VirtIO drivers via pnputil..."
    & pnputil.exe /add-driver (Join-Path $drvRoot '*.inf') /subdirs /install 2>&1 | Out-Null
  }

  # Wait briefly for QEMU SLIRP DHCP IPv4 (works even under restrict=on offline builds)
  Write-Log "Waiting for local virtual NIC IPv4 address..."
  $netOk = $false
  for ($i = 0; $i -lt 30; $i++) {
    try {
      $ip = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' }
      if ($ip) { $netOk = $true; Write-Log ("Network IPv4: " + ($ip.IPAddress -join ', ')); break }
    } catch {}
    Start-Sleep -Seconds 2
  }
  if (-not $netOk) { throw 'Guest virtual NIC did not obtain an IPv4 address' }

  # 3. Install runtime STRAVIA_SEED SSH authorized_keys sync script + ONSTART Scheduled Task
  Write-Log "Installing C:\tools\stravia-seed-sync.ps1 and StraviaSeedSync startup task..."
  New-Item -ItemType Directory -Path 'C:\tools' -Force | Out-Null
  $seedScriptPath = 'C:\tools\stravia-seed-sync.ps1'
  $seedScriptContent = @'
param(
  [string]$Username = 'stravia',
  [string]$SeedFile = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$logPath = 'C:\Windows\Temp\stravia-seed-sync.log'

function Log-Seed([string]$Message) {
  Add-Content -Path $logPath -Value "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $Message" -Encoding UTF8
}

try {
  $src = $null
  if ($SeedFile -and (Test-Path $SeedFile)) {
    $src = $SeedFile
  } else {
    for ($i = 0; $i -lt 60; $i++) {
      $cdDrives = @([System.IO.DriveInfo]::GetDrives() | Where-Object { $_.DriveType -eq [System.IO.DriveType]::CDRom -and $_.IsReady })
      foreach ($d in $cdDrives) {
        $candidate = Join-Path $d.RootDirectory.FullName 'authorized_keys'
        if (($d.VolumeLabel -eq 'STRAVIA_SEED' -or $d.VolumeLabel -eq 'STRAVIA_PROV') -and (Test-Path $candidate)) {
          $src = $candidate
          break
        }
      }
      if (-not $src) {
        foreach ($d in $cdDrives) {
          $candidate = Join-Path $d.RootDirectory.FullName 'authorized_keys'
          if (Test-Path $candidate) {
            $src = $candidate
            break
          }
        }
      }
      if ($src) { break }
      Start-Sleep -Milliseconds 500
    }
  }

  if (-not $src) {
    Log-Seed 'No STRAVIA_SEED authorized_keys found; skipping.'
    exit 0
  }

  $adminAuthKeys = 'C:\ProgramData\ssh\administrators_authorized_keys'
  $userSshDir = "C:\Users\$Username\.ssh"
  $userAuthKeys = Join-Path $userSshDir 'authorized_keys'
  New-Item -ItemType Directory -Path 'C:\ProgramData\ssh', $userSshDir -Force | Out-Null

  Copy-Item -Path $src -Destination $adminAuthKeys -Force
  Copy-Item -Path $src -Destination $userAuthKeys -Force

  & icacls.exe $adminAuthKeys /inheritance:r /grant "SYSTEM:(F)" /grant "Administrators:(F)" | Out-Null
  & icacls.exe $userAuthKeys /inheritance:r /grant "SYSTEM:(F)" /grant "Administrators:(F)" /grant "${Username}:(F)" | Out-Null

  Log-Seed "Synced authorized_keys from $src"
} catch {
  Log-Seed ("ERROR: " + $_.Exception.Message)
  exit 1
}
'@
  Set-Content -Path $seedScriptPath -Value $seedScriptContent -Encoding Ascii

  $prevPref = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    & schtasks.exe /Delete /TN 'StraviaSeedSync' /F 2>&1 | Out-Null
    & schtasks.exe /Create /TN 'StraviaSeedSync' /SC ONSTART /RU SYSTEM /RL HIGHEST /F /TR "powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\tools\stravia-seed-sync.ps1 -Username $Username" 2>&1 | Out-Null
    $seedTaskRc = $LASTEXITCODE
  } finally {
    $ErrorActionPreference = $prevPref
  }
  if ($seedTaskRc -ne 0) { throw "Failed to register StraviaSeedSync scheduled task (exit $seedTaskRc)" }

  # 4. Install Win32-OpenSSH directly under Program Files (inherits clean Program Files ACLs)
  Write-Log "Installing Win32-OpenSSH..."
  $sshDest = 'C:\Program Files\OpenSSH'
  if (Test-Path $sshDest) { & cmd.exe /c "rd /s /q `"$sshDest`"" 2>&1 | Out-Null }
  New-Item -ItemType Directory -Path $sshDest -Force | Out-Null
  & tar.exe -xf (Join-Path $MediaRoot 'tools\openssh.zip') --strip-components=1 -C $sshDest
  if ($LASTEXITCODE -ne 0) { throw "Failed to extract openssh.zip (exit $LASTEXITCODE)" }
  & icacls.exe $sshDest /reset /T /C /Q 2>&1 | Out-Null

  # Register sshd service & start once so sshd natively creates C:\ProgramData\ssh,
  # sshd_config, and host keys with exact official NTFS ACLs.
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $sshDest 'install-sshd.ps1')
  Set-Service -Name sshd -StartupType Automatic
  Start-Service -Name sshd
  for ($w = 0; $w -lt 15; $w++) {
    if ((Test-Path 'C:\ProgramData\ssh\sshd_config') -and (Test-Path 'C:\ProgramData\ssh\ssh_host_ed25519_key')) { break }
    Start-Sleep -Seconds 1
  }
  Stop-Service -Name sshd -Force

  $sshdConfig = 'C:\ProgramData\ssh\sshd_config'
  if (-not (Test-Path $sshdConfig)) {
    Copy-Item -Path (Join-Path $sshDest 'sshd_config_default') -Destination $sshdConfig -Force
  }
  $cfg = Get-Content -Path $sshdConfig -Raw
  $cfg = $cfg -replace '(?m)^#?PubkeyAuthentication\s+.*$', 'PubkeyAuthentication yes'
  $cfg = $cfg -replace '(?m)^#?PasswordAuthentication\s+.*$', 'PasswordAuthentication no'
  $cfg = $cfg -replace '(?m)^#?AuthorizedKeysFile\s+.*$', 'AuthorizedKeysFile .ssh/authorized_keys'
  $cfg = $cfg -replace '(?m)^#?SyslogFacility\s+.*$', 'SyslogFacility LOCAL0'
  Set-Content -Path $sshdConfig -Value $cfg -Encoding Ascii

  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $sshDest 'FixHostFilePermissions.ps1') -Confirm:$false

  # Live-test stravia-seed-sync.ps1 using the build-only authorized_keys on STRAVIA_PROV
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $seedScriptPath -Username $Username
  if ($LASTEXITCODE -ne 0) { throw "stravia-seed-sync.ps1 failed during provisioning (exit $LASTEXITCODE)" }

  $prevPref = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    $sshdTestOut = & (Join-Path $sshDest 'sshd.exe') -t 2>&1
    $sshdTestRc = $LASTEXITCODE
  } finally {
    $ErrorActionPreference = $prevPref
  }
  if ($sshdTestRc -ne 0) {
    throw ("sshd -t config validation failed (exit $sshdTestRc): " + ($sshdTestOut -join ' '))
  }

  Remove-NetFirewallRule -Name 'StraviaOpenSSH' -ErrorAction SilentlyContinue
  New-NetFirewallRule -Name 'StraviaOpenSSH' -DisplayName 'OpenSSH Server (TCP 22)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -Profile Any | Out-Null
  Get-NetConnectionProfile -ErrorAction SilentlyContinue | Set-NetConnectionProfile -NetworkCategory Private -ErrorAction SilentlyContinue

  Start-Service -Name sshd
  Start-Sleep -Seconds 2
  Write-Log ("OpenSSH service status: " + (Get-Service sshd).Status)

  # Fail-fast verify TCP 22 serves an OpenSSH banner on loopback
  $sshBanner = $null
  for ($attempt = 1; $attempt -le 10; $attempt++) {
    try {
      $tcp = New-Object System.Net.Sockets.TcpClient
      $tcp.ReceiveTimeout = 3000
      $tcp.SendTimeout = 3000
      $tcp.Connect('127.0.0.1', 22)
      $reader = New-Object System.IO.StreamReader($tcp.GetStream())
      $sshBanner = $reader.ReadLine()
      $tcp.Close()
      if ($sshBanner -and $sshBanner.StartsWith('SSH-2.0-')) { break }
    } catch {
      Start-Sleep -Seconds 1
    }
  }
  if (-not ($sshBanner -and $sshBanner.StartsWith('SSH-2.0-'))) {
    $sshLog = if (Test-Path 'C:\ProgramData\ssh\logs\sshd.log') { (Get-Content 'C:\ProgramData\ssh\logs\sshd.log' -Tail 20) -join ' | ' } else { '' }
    $net22 = & netstat.exe -ano | Select-String ':22 '
    throw ("OpenSSH failed banner self-check (banner='$sshBanner'; netstat='$($net22 -join '; ')'; log='$sshLog')")
  }
  Write-Log ("OpenSSH banner verified: $sshBanner")

  # 5. Install Node.js, Bun, uv, and offline standalone Python 3.12 into C:\tools
  Write-Log "Installing Node.js, Bun, uv, and standalone Python 3.12 into C:\tools..."
  New-Item -ItemType Directory -Path 'C:\tools\node', 'C:\tools\bun', 'C:\tools\uv', 'C:\tools\python312', 'C:\tools\bun-cache', 'C:\tools\uv-cache', 'C:\Windows\Temp\bun-extract', 'C:\Windows\Temp\uv-extract' -Force | Out-Null

  & tar.exe -xf (Join-Path $MediaRoot 'tools\node.zip') --strip-components=1 -C 'C:\tools\node'
  if ($LASTEXITCODE -ne 0) { throw "Failed to extract node.zip (exit $LASTEXITCODE)" }

  & tar.exe -xf (Join-Path $MediaRoot 'tools\bun.zip') -C 'C:\Windows\Temp\bun-extract'
  if ($LASTEXITCODE -ne 0) { throw "Failed to extract bun.zip (exit $LASTEXITCODE)" }
  $bunExe = Get-ChildItem 'C:\Windows\Temp\bun-extract' -Recurse -Filter 'bun.exe' | Select-Object -First 1
  Copy-Item -Path $bunExe.FullName -Destination 'C:\tools\bun\bun.exe' -Force

  & tar.exe -xf (Join-Path $MediaRoot 'tools\uv.zip') -C 'C:\Windows\Temp\uv-extract'
  if ($LASTEXITCODE -ne 0) { throw "Failed to extract uv.zip (exit $LASTEXITCODE)" }
  Get-ChildItem 'C:\Windows\Temp\uv-extract' -Recurse -Filter '*.exe' | ForEach-Object {
    Copy-Item -Path $_.FullName -Destination 'C:\tools\uv\' -Force
  }

  & tar.exe -xzf (Join-Path $MediaRoot 'tools\python312.tar.gz') --strip-components=1 -C 'C:\tools\python312'
  if ($LASTEXITCODE -ne 0) { throw "Failed to extract python312.tar.gz (exit $LASTEXITCODE)" }
  if (-not (Test-Path 'C:\tools\python312\python.exe')) {
    throw 'Standalone Python extraction did not produce C:\tools\python312\python.exe'
  }

  & icacls.exe 'C:\tools' /reset /T /C /Q 2>&1 | Out-Null

  $extraPath = 'C:\tools\node;C:\tools\bun;C:\tools\uv;C:\tools\python312;C:\tools\python312\Scripts;C:\Program Files\OpenSSH'
  $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
  [Environment]::SetEnvironmentVariable('Path', "$extraPath;$machinePath", 'Machine')
  [Environment]::SetEnvironmentVariable('BUN_INSTALL_CACHE_DIR', 'C:\tools\bun-cache', 'Machine')
  [Environment]::SetEnvironmentVariable('UV_CACHE_DIR', 'C:\tools\uv-cache', 'Machine')
  [Environment]::SetEnvironmentVariable('UV_PYTHON', '3.12', 'Machine')
  [Environment]::SetEnvironmentVariable('UV_PYTHON_PREFERENCE', 'only-system', 'Machine')
  [Environment]::SetEnvironmentVariable('UV_PYTHON_DOWNLOADS', 'never', 'Machine')
  $env:Path = "$extraPath;$env:Path"
  $env:BUN_INSTALL_CACHE_DIR = 'C:\tools\bun-cache'
  $env:UV_CACHE_DIR = 'C:\tools\uv-cache'
  $env:UV_PYTHON = '3.12'
  $env:UV_PYTHON_PREFERENCE = 'only-system'
  $env:UV_PYTHON_DOWNLOADS = 'never'

  $nodeVer = (& C:\tools\node\node.exe --version)
  $bunVer = (& C:\tools\bun\bun.exe --version)
  $uvVer = (& C:\tools\uv\uv.exe --version)
  $pyVer = (& C:\tools\python312\python.exe --version)
  $uvPyPath = (& C:\tools\uv\uv.exe python find 3.12)
  if ($LASTEXITCODE -ne 0 -or -not $uvPyPath) { throw "uv failed to discover offline Python 3.12 (exit $LASTEXITCODE)" }
  Write-Log ("node=$nodeVer; bun=$bunVer; uv=$uvVer; python=$pyVer; uv_python_find=$uvPyPath")

  # Configure Defender exclusions / disable real-time scanning for isolated E2E VM
  Write-Log "Configuring Windows Defender exclusions for isolated E2E VM..."
  try { Add-MpPreference -ExclusionPath 'C:\stravia-e2e', 'C:\tools', 'C:\Windows\Temp' -ErrorAction SilentlyContinue } catch {}
  try { Set-MpPreference -DisableRealtimeMonitoring $true -DisableBehaviorMonitoring $true -DisableIOAVProtection $true -DisableScriptScanning $true -ErrorAction SilentlyContinue } catch {}
  try {
    New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender' -Force -ErrorAction SilentlyContinue | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender' -Name 'DisableAntiSpyware' -Value 1 -Type DWord -ErrorAction SilentlyContinue
    New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection' -Force -ErrorAction SilentlyContinue | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection' -Name 'DisableRealtimeMonitoring' -Value 1 -Type DWord -ErrorAction SilentlyContinue
  } catch {}

  # 6. Install Microsoft Edge WebView2 Evergreen Standalone Runtime (100% offline)
  Write-Log "Installing Microsoft Edge WebView2 Standalone Runtime (offline)..."
  $wv2Installer = 'C:\Windows\Temp\MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
  Copy-Item -Path (Join-Path $MediaRoot 'tools\MicrosoftEdgeWebView2RuntimeInstallerX64.exe') -Destination $wv2Installer -Force
  $wv2Proc = Start-Process -FilePath $wv2Installer -ArgumentList '/silent', '/install' -Wait -PassThru
  Write-Log "WebView2 standalone installer exit code: $($wv2Proc.ExitCode)"
  Remove-Item -Force -ErrorAction SilentlyContinue $wv2Installer
  $wv2Exe = Get-ChildItem 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application' -Recurse -Filter 'msedgewebview2.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $wv2Exe) { throw 'WebView2 Standalone Runtime installation did not produce msedgewebview2.exe' }
  Write-Log ("WebView2 verified: " + $wv2Exe.FullName + " v" + $wv2Exe.VersionInfo.ProductVersion)

  # 7. Secondary trimming (tiny11maker-style, adapted for Win10 Enterprise LTSC 2021 Eval)
  Write-Log "=== Starting secondary trimming (preserving WebView2/WMI/PS/OpenSSH/DWM) ==="

  # 7a. Remove full Edge browser while strictly preserving EdgeWebView2 Runtime
  $edgeApp = 'C:\Program Files (x86)\Microsoft\Edge\Application'
  if (Test-Path $edgeApp) {
    $edgeSetup = Get-ChildItem $edgeApp -Recurse -Filter 'setup.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($edgeSetup) {
      Write-Log "Uninstalling full Edge browser via $($edgeSetup.FullName)..."
      Start-Process -FilePath $edgeSetup.FullName -ArgumentList '--uninstall', '--system-level', '--verbose-logging', '--force-uninstall' -Wait -ErrorAction SilentlyContinue
    }
    if (Test-Path 'C:\Program Files (x86)\Microsoft\Edge') {
      try { & cmd.exe /c 'rd /s /q "C:\Program Files (x86)\Microsoft\Edge"' 2>&1 | Out-Null } catch {}
    }
    if (Test-Path 'C:\Program Files (x86)\Microsoft\EdgeCore') {
      try { & cmd.exe /c 'rd /s /q "C:\Program Files (x86)\Microsoft\EdgeCore"' 2>&1 | Out-Null } catch {}
    }
  }

  # 7b. Remove non-essential Windows capabilities
  $capPatterns = @(
    'App.StepsRecorder*',
    'App.Support.QuickAssist*',
    'Browser.InternetExplorer*',
    'Hello.Face*',
    'MathRecognizer*',
    'Media.WindowsMediaPlayer*',
    'OneCoreUAP.OneSync*',
    'OpenSSH.Client*',
    'Print.Fax.Scan*'
  )
  $installedCaps = Get-WindowsCapability -Online | Where-Object { $_.State -eq 'Installed' }
  foreach ($cap in $installedCaps) {
    foreach ($pat in $capPatterns) {
      if ($cap.Name -like $pat) {
        Write-Log "Removing capability: $($cap.Name)"
        try {
          Remove-WindowsCapability -Online -Name $cap.Name -ErrorAction Stop | Out-Null
        } catch {
          Write-Log "Warning: failed to remove capability $($cap.Name): $($_.Exception.Message)"
        }
        break
      }
    }
  }

  # 7c. Disable background services that add CI jitter/IO overhead (keeping Winmgmt/Schedule/sshd/Lanman/DWM intact)
  $disableServices = @(
    'wuauserv', 'UsoSvc', 'WSearch', 'SysMain', 'DiagTrack', 'dmwappushservice',
    'Spooler', 'Fax', 'MapsBroker', 'WbioSrvc', 'TabletInputService', 'WerSvc',
    'PcaSvc', 'RemoteRegistry', 'RetailDemo', 'WMPNetworkSvc', 'XblAuthManager',
    'XblGameSave', 'XboxNetApiSvc', 'XboxGipSvc'
  )
  foreach ($svcName in $disableServices) {
    $svc = Get-Service -Name $svcName -ErrorAction SilentlyContinue
    if ($svc) {
      try { Stop-Service -Name $svcName -Force -ErrorAction SilentlyContinue } catch {}
      try { Set-Service -Name $svcName -StartupType Disabled -ErrorAction SilentlyContinue } catch {}
    }
  }

  try {
    Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl' -Name 'CrashDumpEnabled' -Value 0 -Type DWord -ErrorAction SilentlyContinue
    New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection' -Force -ErrorAction SilentlyContinue | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection' -Name 'AllowTelemetry' -Value 0 -Type DWord -ErrorAction SilentlyContinue
  } catch {}

  # 7d. Clean WinSxS component store
  Write-Log "Running DISM StartComponentCleanup /ResetBase..."
  $prevPref = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    & dism.exe /Online /Cleanup-Image /StartComponentCleanup /ResetBase 2>&1 | Out-Null
    Write-Log "DISM cleanup exit code: $LASTEXITCODE"
  } finally {
    $ErrorActionPreference = $prevPref
  }

  # 7e. Enable CompactOS compression
  Write-Log "Enabling CompactOS..."
  $prevPref = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    & compact.exe /CompactOS:always 2>&1 | Out-Null
    Write-Log "CompactOS exit code: $LASTEXITCODE"
  } finally {
    $ErrorActionPreference = $prevPref
  }

  # 7f. Purge temporary files & logs
  Write-Log "Cleaning temporary directories..."
  $purgeDirs = @(
    'C:\Windows\Temp\bun-extract',
    'C:\Windows\Temp\uv-extract'
  )
  foreach ($d in $purgeDirs) {
    if (Test-Path $d) { try { & cmd.exe /c "rd /s /q `"$d`"" 2>&1 | Out-Null } catch {} }
  }
  $purgePaths = @(
    'C:\Windows\SoftwareDistribution\Download\*',
    'C:\Windows\Prefetch\*',
    "C:\Users\$Username\AppData\Local\Temp\*"
  )
  foreach ($p in $purgePaths) {
    try { Remove-Item -Path $p -Recurse -Force -ErrorAction SilentlyContinue } catch {}
  }

  # 8. Post-trim integrity self-check
  Write-Log "=== Running post-trim integrity self-checks ==="
  $wv2After = Get-ChildItem 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application' -Recurse -Filter 'msedgewebview2.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $wv2After) { throw 'Post-trim check FAILED: msedgewebview2.exe missing!' }
  $osInfo = Get-CimInstance Win32_OperatingSystem
  if (-not $osInfo) { throw 'Post-trim check FAILED: WMI Get-CimInstance broken!' }
  $sshdSvc = Get-Service sshd
  if ($sshdSvc.Status -ne 'Running') { throw 'Post-trim check FAILED: sshd is not running!' }
  if (-not (Test-Path $seedScriptPath)) { throw 'Post-trim check FAILED: stravia-seed-sync.ps1 missing!' }
  $uvPyAfter = (& C:\tools\uv\uv.exe python find 3.12)
  if ($LASTEXITCODE -ne 0 -or -not $uvPyAfter) { throw 'Post-trim check FAILED: uv cannot find offline Python 3.12!' }

  # Wait up to 60s for host->guest SSH auth self-test marker before wiping the ephemeral build key
  $sshFlag = 'C:\Windows\Temp\host-ssh-verified.flag'
  for ($w = 0; $w -lt 60; $w++) {
    if (Test-Path $sshFlag) { break }
    Start-Sleep -Seconds 1
  }
  if (-not (Test-Path $sshFlag)) {
    throw 'Timed out waiting for host->guest SSH verification marker (C:\Windows\Temp\host-ssh-verified.flag)'
  }
  Remove-Item -Force -ErrorAction SilentlyContinue $sshFlag

  # Seal golden image: remove build-only SSH authorized_keys so zero static keys remain in image
  Remove-Item -Force -ErrorAction SilentlyContinue 'C:\ProgramData\ssh\administrators_authorized_keys', "C:\Users\$Username\.ssh\authorized_keys"
  Write-Log "Sealed image: removed ephemeral build SSH keys (runtime keys injected via STRAVIA_SEED ISO)."

  $usedAfter = [math]::Round((Get-PSDrive C).Used / 1GB, 2)
  $summary = @(
    "os=$($osInfo.Caption) ($($osInfo.Version))",
    "webview2=$($wv2After.VersionInfo.ProductVersion)",
    "node=$(& C:\tools\node\node.exe --version)",
    "bun=$(& C:\tools\bun\bun.exe --version)",
    "uv=$(& C:\tools\uv\uv.exe --version)",
    "python=$(& C:\tools\python312\python.exe --version)",
    "disk_used_before_gb=$freeBefore",
    "disk_used_after_trim_gb=$usedAfter"
  )
  Set-Content -Path 'C:\stravia-image-ready.txt' -Value $summary -Encoding UTF8
  Write-Log ("Post-trim self-check PASSED: " + ($summary -join '; '))

  # 9. Zero free NTFS clusters natively via .NET FileStream (no external SDelete binary needed)
  Write-Log "Zeroing free space on C: natively for optimal zstd qcow2 compression..."
  $zeroFile = 'C:\stravia-zero.tmp'
  $buf = New-Object byte[] (16MB)
  $written = [int64]0
  $fs = $null
  try {
    $fs = [System.IO.File]::Open($zeroFile, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    while ($true) {
      $freeBytes = [int64]([System.IO.DriveInfo]'C').AvailableFreeSpace
      if ($freeBytes -le 64MB) { break }
      $remaining = [int64]($freeBytes - 32MB)
      if ($remaining -lt $buf.Length) { $chunk = [int]$remaining } else { $chunk = $buf.Length }
      if ($chunk -le 0) { break }
      $fs.Write($buf, 0, $chunk)
      $written += $chunk
    }
    $fs.Flush($true)
  } catch [System.IO.IOException] {
    # Expected when nearing disk capacity
  } finally {
    if ($fs) { $fs.Dispose() }
    if (Test-Path $zeroFile) { Remove-Item -Force -ErrorAction SilentlyContinue $zeroFile }
  }
  Write-Log ("Zeroed " + [math]::Round($written / 1GB, 2) + " GiB of free space on C:")

  Write-Log "STRAVIA_IMAGE_BUILD_COMPLETE"
  if ($script:ComPort -and $script:ComPort.IsOpen) { $script:ComPort.Close() }
  & shutdown.exe /s /t 5 /f
} catch {
  Write-Log ("STRAVIA_IMAGE_BUILD_FAILED: " + $_.Exception.Message)
  Write-Log $_.ScriptStackTrace
  if ($script:ComPort -and $script:ComPort.IsOpen) { $script:ComPort.Close() }
  exit 1
}
PSEOF

PROV_ISO="${WORK_DIR}/provision.iso"
echo "==> Building provision.iso..."
if [[ "$(basename "${MKISOFS_BIN}")" == "xorriso" ]]; then
  "${MKISOFS_BIN}" -as mkisofs -J -R -V "STRAVIA_PROV" -o "${PROV_ISO}" "${PROV_DIR}" >/dev/null 2>&1
else
  "${MKISOFS_BIN}" -J -R -V "STRAVIA_PROV" -o "${PROV_ISO}" "${PROV_DIR}" >/dev/null 2>&1
fi

RAW_IMG="${WORK_DIR}/win10-ltsc-raw.qcow2"
SERIAL_LOG="${WORK_DIR}/serial.log"
MON_SOCK="${WORK_DIR}/qemu-monitor.sock"
SSH_PORT="$((22000 + RANDOM % 10000))"

echo "==> Creating ${DISK_SIZE} virtual disk at ${RAW_IMG}..."
"${QEMU_IMG_BIN}" create -f qcow2 "${RAW_IMG}" "${DISK_SIZE}" >/dev/null

NETDEV_ARG="user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22"
if [[ "${OFFLINE_BUILD}" == "1" ]]; then
  NETDEV_ARG="user,id=net0,restrict=on,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22"
fi

echo "==> Booting QEMU/KVM for unattended Win10 LTSC 2021 Eval install + trim (disk=${DISK_SIZE}, offline=${OFFLINE_BUILD})..."
echo "    Serial log: ${SERIAL_LOG}"
echo "    Forwarded SSH during build: 127.0.0.1:${SSH_PORT}"
"${QEMU_BIN}" \
  -enable-kvm \
  -machine q35,accel=kvm \
  -cpu host,hv_relaxed,hv_spinlocks=0x1fff,hv_vapic,hv_time \
  -smp "${BUILD_CPUS}" \
  -m "${BUILD_RAM}" \
  -boot order=cd,menu=off \
  -drive "file=${RAW_IMG},format=qcow2,if=virtio,cache=writeback,discard=unmap" \
  -drive "file=${WIN10_LTSC_ISO_PATH},media=cdrom,index=0,readonly=on" \
  -drive "file=${PROV_ISO},media=cdrom,index=1,readonly=on" \
  -netdev "${NETDEV_ARG}" \
  -device virtio-net-pci,netdev=net0 \
  -device VGA,vgamem_mb=32 \
  -display none \
  -monitor "unix:${MON_SOCK},server,nowait" \
  -serial "file:${SERIAL_LOG}" &
QEMU_PID=$!

# Send a Return keystroke a few seconds after BIOS boot in case Windows Setup shows
# "Press any key to boot from CD or DVD..." on the initial boot.
(
  sleep 4
  for _ in 1 2 3; do
    if [[ -S "${MON_SOCK}" ]]; then
      "${PYTHON3_BIN}" -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(b"sendkey ret\n"); s.close()' "${MON_SOCK}" 2>/dev/null || true
    fi
    sleep 2
  done
) &

START_TS="$(date +%s)"
LAST_SERIAL_LINE=""
HOST_SSH_CHECKED=0
while kill -0 "${QEMU_PID}" 2>/dev/null; do
  NOW_TS="$(date +%s)"
  ELAPSED=$((NOW_TS - START_TS))
  if (( ELAPSED > BUILD_TIMEOUT )); then
    echo "ERROR: Windows golden image build timed out after ${BUILD_TIMEOUT}s" >&2
    if [[ -f "${SERIAL_LOG}" ]]; then
      echo "--- Last 40 lines of serial.log ---" >&2
      tail -n 40 "${SERIAL_LOG}" >&2
    fi
    exit 1
  fi
  if [[ -f "${SERIAL_LOG}" ]]; then
    CUR_LINE="$(tail -n 1 "${SERIAL_LOG}" 2>/dev/null || true)"
    if [[ -n "${CUR_LINE}" && "${CUR_LINE}" != "${LAST_SERIAL_LINE}" ]]; then
      echo "  [${ELAPSED}s] ${CUR_LINE}"
      LAST_SERIAL_LINE="${CUR_LINE}"
    fi
    if [[ "${HOST_SSH_CHECKED}" == "0" ]] && grep -q "OpenSSH banner verified:" "${SERIAL_LOG}" 2>/dev/null; then
      HOST_SSH_CHECKED=1
      SSH_VERIFY_OK=0
      for _ in 1 2 3; do
        if ssh -F /dev/null -i "${BUILD_SSH_KEY}" -p "${SSH_PORT}" \
          -o ControlMaster=no -o ControlPath=none -o IdentitiesOnly=yes \
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
          -o BatchMode=yes -o ConnectTimeout=5 \
          "${WINDOWS_SSH_USER}@127.0.0.1" \
          'powershell -NoProfile -NonInteractive -Command "Set-Content -Path C:\Windows\Temp\host-ssh-verified.flag -Value ok -Encoding Ascii; Write-Output HOST_SSH_AUTH_OK"' \
          2>&1 | grep -q "HOST_SSH_AUTH_OK"; then
          SSH_VERIFY_OK=1
          break
        fi
        sleep 2
      done
      if [[ "${SSH_VERIFY_OK}" == "1" ]]; then
        echo "  [${ELAPSED}s] Host->Guest OpenSSH public-key authentication VERIFIED!"
      else
        echo "ERROR: Host->Guest OpenSSH public-key authentication FAILED during build!" >&2
        ssh -vv -F /dev/null -i "${BUILD_SSH_KEY}" -p "${SSH_PORT}" \
          -o ControlMaster=no -o ControlPath=none -o IdentitiesOnly=yes \
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
          -o BatchMode=yes -o ConnectTimeout=5 \
          "${WINDOWS_SSH_USER}@127.0.0.1" "cmd /c echo test" 2>&1 | tail -n 25 >&2 || true
        exit 1
      fi
    fi
    if grep -q "STRAVIA_IMAGE_BUILD_FAILED" "${SERIAL_LOG}" 2>/dev/null; then
      echo "ERROR: Provisioning script failed inside Windows VM:" >&2
      cat "${SERIAL_LOG}" >&2
      exit 1
    fi
  fi
  sleep 5
done

wait "${QEMU_PID}" || true
QEMU_PID=""

if ! grep -q "STRAVIA_IMAGE_BUILD_COMPLETE" "${SERIAL_LOG}" 2>/dev/null; then
  echo "ERROR: QEMU exited without STRAVIA_IMAGE_BUILD_COMPLETE marker in ${SERIAL_LOG}" >&2
  [[ -f "${SERIAL_LOG}" ]] && cat "${SERIAL_LOG}" >&2
  exit 1
fi
if [[ "${HOST_SSH_CHECKED}" != "1" ]]; then
  echo "ERROR: Build completed without running host->guest SSH verification!" >&2
  exit 1
fi

echo "==> Provisioning & trimming completed cleanly! Serial summary:"
cat "${SERIAL_LOG}"

TMP_OUT="${OUTPUT_IMAGE}.tmp"
rm -f "${TMP_OUT}"
echo "==> Compressing golden image with zstd (cluster_size=65536) -> ${OUTPUT_IMAGE}..."
"${QEMU_IMG_BIN}" convert -p -O qcow2 -c -o compression_type=zstd,cluster_size=65536 "${RAW_IMG}" "${TMP_OUT}"
chmod 0444 "${TMP_OUT}"
mv -f "${TMP_OUT}" "${OUTPUT_IMAGE}"

if [[ -n "${METADATA_OUTPUT}" ]]; then
  BUILD_DATE="$(date -u +%Y-%m-%d)"
  SUMMARY_LINE="$(grep 'Post-trim self-check PASSED:' "${SERIAL_LOG}" | tail -n 1 || true)"
  "${PYTHON3_BIN}" - "${METADATA_OUTPUT}" "${BUILD_DATE}" "${DISK_SIZE}" "${WINDOWS_SSH_USER}" "${OPENSSH_VERSION}" "${SUMMARY_LINE}" <<'PYEOF'
import json, re, sys
out_path, build_date, disk_size, ssh_user, openssh_ver, summary_line = sys.argv[1:7]
meta = {
    "buildDate": build_date,
    "diskSize": disk_size,
    "sshUser": ssh_user,
    "sshAuthMode": "seed-iso",
    "openssh": openssh_ver,
}
m = re.search(r"Post-trim self-check PASSED:\s*(.+)$", summary_line)
if m:
    for item in m.group(1).split(";"):
        if "=" in item:
            k, v = item.split("=", 1)
            meta[k.strip()] = v.strip()
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
    f.write("\n")
PYEOF
  chmod 0444 "${METADATA_OUTPUT}"
fi

echo "==> Golden read-only image ready:"
ls -lh "${OUTPUT_IMAGE}"
"${QEMU_IMG_BIN}" info "${OUTPUT_IMAGE}"
echo "SSH user: ${WINDOWS_SSH_USER} (runtime keys injected dynamically via STRAVIA_SEED ISO)"
