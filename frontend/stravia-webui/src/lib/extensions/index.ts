import { registerBuildExtensions } from '#webui-extensions'
import { webUiExtensions } from './registry.js'

let initialized = false

export function ensureBuildExtensionsRegistered(): typeof webUiExtensions {
  if (!initialized) {
    registerBuildExtensions(webUiExtensions)
    initialized = true
  }
  return webUiExtensions
}

export * from './registry.js'
