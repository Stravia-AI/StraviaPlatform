import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'

const webRoot = fileURLToPath(new URL('../', import.meta.url))
const repositoryRoot = resolve(webRoot, '../..')
const master = readFileSync(join(webRoot, 'src/assets/logos/stravia-logo.svg'), 'utf8').trim()
const body = master.slice(master.indexOf('>') + 1, master.lastIndexOf('</svg>'))
const source = '<!-- Generated from src/assets/logos/stravia-logo.svg by task brand:generate. -->\n'
const document = (content: string, viewBox = '0 0 448 448') =>
  `${source}<svg xmlns="http://www.w3.org/2000/svg" viewBox="${viewBox}" role="img" aria-label="Stravia">${content}</svg>\n`
const staticRoot = join(webRoot, 'static')
writeFileSync(join(staticRoot, 'stravia-logo.svg'), document(body.replaceAll('currentColor', '#151515')))
writeFileSync(join(staticRoot, 'stravia-logo-reversed.svg'), document(body.replaceAll('currentColor', '#ffffff')))
writeFileSync(
  join(staticRoot, 'stravia-favicon.svg'),
  document(
    '<style>svg{color:#151515}@media(prefers-color-scheme:dark){svg{color:#ffffff}}</style>' + body,
    '24 24 400 400',
  ),
)
const appIcon = document(body.replaceAll('currentColor', '#000000'))
const darkAppIcon = document(body.replaceAll('currentColor', '#ffffff'))
const appIconPath = join(staticRoot, 'stravia-app-icon.svg')
writeFileSync(appIconPath, appIcon)
const darkAppIconPath = join(staticRoot, 'stravia-app-icon-dark.svg')
writeFileSync(darkAppIconPath, darkAppIcon)

// Tauri's native window, tray and bundle icons need raster/container formats.
const scratchRoot = join(repositoryRoot, '.scratch')
mkdirSync(scratchRoot, { recursive: true })
const temporary = mkdtempSync(join(scratchRoot, 'brand-icons-'))
if (dirname(resolve(temporary)) !== resolve(scratchRoot)) throw new Error('Unsafe icon cleanup path')
try {
  const taskfile = readFileSync(join(repositoryRoot, 'Taskfile.yml'), 'utf8')
  const version = taskfile.match(/TAURI_CLI_VERSION:\s*([^\s]+)/)?.[1]
  if (!version) throw new Error('Taskfile.yml must pin TAURI_CLI_VERSION')
  const generate = (input: string, output: string) =>
    execFileSync('bunx', [`@tauri-apps/cli@${version}`, 'icon', input, '--output', output], {
      cwd: repositoryRoot,
      stdio: 'inherit',
    })
  generate(appIconPath, temporary)
  const nativeRoot = join(repositoryRoot, 'backend/apps/stravia-desktop/icons')
  for (const file of ['32x32.png', '64x64.png', '128x128.png', '128x128@2x.png', 'icon.png', 'icon.ico', 'icon.icns']) {
    cpSync(join(temporary, file), join(nativeRoot, file))
  }
  const darkOutput = join(temporary, 'dark')
  generate(darkAppIconPath, darkOutput)
  for (const size of [32, 128]) {
    cpSync(join(temporary, `${size}x${size}.png`), join(nativeRoot, `system-light-${size}.png`))
    cpSync(join(darkOutput, `${size}x${size}.png`), join(nativeRoot, `system-dark-${size}.png`))
  }
} finally {
  rmSync(temporary, { recursive: true })
}
