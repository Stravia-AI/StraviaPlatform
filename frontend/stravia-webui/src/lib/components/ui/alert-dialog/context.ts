import { getContext, setContext } from 'svelte'

export interface AlertDialogContextValue {
  close: () => void
}

const alertDialogContextKey = Symbol('alert-dialog')

export function setAlertDialogContext(value: AlertDialogContextValue): void {
  setContext(alertDialogContextKey, value)
}

export function getAlertDialogContext(): AlertDialogContextValue | undefined {
  return getContext<AlertDialogContextValue | undefined>(alertDialogContextKey)
}
