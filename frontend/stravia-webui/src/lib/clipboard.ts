export async function copyText(value: string, trigger: HTMLElement): Promise<void> {
  const clipboard = navigator.clipboard
  if (typeof clipboard?.writeText === 'function') return clipboard.writeText(value)

  // Keep the fallback synchronous in the click and inside the trigger's focus trap.
  const doc = trigger.ownerDocument
  const previousFocus = doc.activeElement
  const textarea = doc.createElement('textarea')
  textarea.value = value
  textarea.readOnly = true
  textarea.tabIndex = -1
  textarea.style.cssText = 'position: fixed; top: 0; left: -9999px; opacity: 0; pointer-events: none;'

  try {
    trigger.after(textarea)
    textarea.focus({ preventScroll: true })
    textarea.select()
    if (doc.activeElement !== textarea || !doc.execCommand('copy')) {
      throw new Error('Could not copy to clipboard')
    }
  } finally {
    textarea.remove()
    const focusTarget =
      previousFocus instanceof HTMLElement && previousFocus !== doc.body && previousFocus.isConnected
        ? previousFocus
        : trigger
    focusTarget.focus({ preventScroll: true })
  }
}
