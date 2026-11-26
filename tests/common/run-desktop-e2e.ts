import { spawnSync } from 'node:child_process'

const [command, ...args] = process.argv.slice(2)
if (!command) throw new Error('Usage: bun tests/common/run-desktop-e2e.ts <command> [args...]')

let finalCommand = command
let finalArgs = args
const env: NodeJS.ProcessEnv = { ...process.env }

if (process.platform === 'linux') {
  const headless = !env.DISPLAY && !env.WAYLAND_DISPLAY
  if (headless) {
    // 无 GPU 的虚拟帧缓冲下关闭 DMABUF 渲染器，避免 WebKitGTK EGL/GBM 初始化失败。
    env.WEBKIT_DISABLE_DMABUF_RENDERER ??= '1'
  }

  if (!env.DBUS_SESSION_BUS_ADDRESS && Bun.which('dbus-run-session')) {
    finalArgs = [finalCommand, ...finalArgs]
    finalCommand = 'dbus-run-session'
  }

  if (headless) {
    if (!Bun.which('xvfb-run')) {
      throw new Error('Headless Linux desktop E2E requires xvfb-run (or an active DISPLAY / WAYLAND_DISPLAY)')
    }
    // 窗口默认 1440x900，Xvfb 屏幕需大于该尺寸；启动轻量 WM 以保证窗口最大化与前台焦点事件正常。
    const innerScript = [
      'wm_pid=""',
      'cleanup() { if [ -n "$wm_pid" ]; then kill "$wm_pid" 2>/dev/null || true; wait "$wm_pid" 2>/dev/null || true; fi; }',
      'trap cleanup EXIT',
      'if command -v openbox >/dev/null 2>&1; then openbox >/dev/null 2>&1 & wm_pid=$!; sleep 0.3; fi',
      '"$@"',
    ].join('\n')
    finalArgs = ['-a', '-s', '-screen 0 1920x1080x24', 'sh', '-c', innerScript, '_', finalCommand, ...finalArgs]
    finalCommand = 'xvfb-run'
  }
}

const result = spawnSync(finalCommand, finalArgs, { env, stdio: 'inherit' })
if (result.error) throw result.error
process.exitCode = result.status ?? 1
