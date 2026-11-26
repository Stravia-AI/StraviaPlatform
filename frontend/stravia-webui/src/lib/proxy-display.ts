import type { OutboundProxyEntry } from '#lib/admin-client.js'

/** Remove embedded credentials before displaying a proxy URL in the UI. */
export function redactProxyUrl(raw: string): string {
  const trimmed = raw.trim()
  if (!trimmed) return ''
  try {
    const parsed = new URL(trimmed)
    parsed.username = ''
    parsed.password = ''
    const formatted = parsed.toString()
    return formatted.endsWith('/') && !trimmed.endsWith('/') ? formatted.slice(0, -1) : formatted
  } catch {
    return trimmed.replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/@]*@/i, '$1')
  }
}

/** Extract host[:port] without credentials for unnamed proxy fallback labels. */
export function proxyHostPort(raw: string): string {
  const trimmed = raw.trim()
  if (!trimmed) return ''
  try {
    const parsed = new URL(trimmed)
    return parsed.host || redactProxyUrl(trimmed)
  } catch {
    return redactProxyUrl(trimmed)
  }
}

/** Display label for a saved proxy entry: custom name, or host:port when unnamed. */
export function proxyDisplayName(entry: Pick<OutboundProxyEntry, 'name' | 'url'>): string {
  const trimmed = entry.name.trim()
  return trimmed || proxyHostPort(entry.url)
}
