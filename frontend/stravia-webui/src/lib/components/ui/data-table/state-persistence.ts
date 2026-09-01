import type { DataTableState } from './data-table.js'

const DATA_TABLE_STATE_VERSION = 1

export function parseDataTableState(raw: string, stateKey: string): Partial<DataTableState> {
  const saved = JSON.parse(raw) as { version?: unknown; state?: Partial<DataTableState> }
  if (saved.version !== DATA_TABLE_STATE_VERSION || !saved.state || typeof saved.state !== 'object') {
    throw new Error(`Unsupported DataTable state stored under "${stateKey}".`)
  }
  return saved.state
}

export function serializeDataTableState(state: DataTableState): string {
  return JSON.stringify({ version: DATA_TABLE_STATE_VERSION, state })
}
