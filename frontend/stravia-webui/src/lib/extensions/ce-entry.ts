import type { WebUiExtensionRegistry } from './registry.js'

/** Community Edition build-time extension entry: registers zero commercial modules. */
export function registerBuildExtensions(registry: WebUiExtensionRegistry): void {
  void registry
}
