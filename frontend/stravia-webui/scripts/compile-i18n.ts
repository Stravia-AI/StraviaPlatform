import { copyFile } from 'node:fs/promises'
import { resolve } from 'node:path'

const webuiRoot = resolve(import.meta.dir, '..')
const projectPath = resolve(webuiRoot, 'project.inlang')
const outputPath = resolve(webuiRoot, 'src/lib/paraglide')

await copyFile(
  Bun.resolveSync('@inlang/plugin-message-format', webuiRoot),
  resolve(webuiRoot, 'node_modules/.stravia-inlang-message-format.js'),
)

const compiler = Bun.spawn(
  [
    process.execPath,
    'run',
    'paraglide-js',
    'compile',
    '--project',
    projectPath,
    '--outdir',
    outputPath,
    '--strategy',
    'globalVariable',
    'baseLocale',
    '--emit-ts-declarations',
    '--no-emit-readme',
  ],
  {
    cwd: webuiRoot,
    stdout: 'inherit',
    stderr: 'inherit',
  },
)

const exitCode = await compiler.exited
if (exitCode !== 0) {
  process.exit(exitCode)
}

const requiredOutputs = ['messages.js', 'runtime.js']
const missingOutputs = (
  await Promise.all(
    requiredOutputs.map(async (name) => ({
      name,
      exists: await Bun.file(resolve(outputPath, name)).exists(),
    })),
  )
)
  .filter(({ exists }) => !exists)
  .map(({ name }) => name)

if (missingOutputs.length > 0) {
  throw new Error(`Paraglide did not generate required outputs: ${missingOutputs.join(', ')}`)
}
