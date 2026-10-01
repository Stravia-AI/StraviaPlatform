import * as m from '$lib/paraglide/messages.js'

export interface StartupProgress {
  phase: string
  label: string
  completed: number
  total: number | null
}

export function startupPhaseLabel(progress: StartupProgress): string {
  switch (progress.phase) {
    case 'data_directory':
      return m.desktop_startup_stage_data_directory()
    case 'gateway':
      return m.desktop_startup_stage_gateway()
    case 'session':
      return m.startup_phase_session()
    case 'http':
      return m.desktop_startup_stage_http()
    case 'desktop':
      return m.desktop_startup_stage_desktop()
    case 'storage_connect':
      return m.startup_phase_storage_connect()
    case 'migration_validate':
      return m.startup_phase_migration_validate()
    case 'history_schema':
      return m.startup_phase_history_schema()
    case 'history_data':
      return m.startup_phase_history_data()
    case 'debug_manifests':
      return m.startup_phase_debug_manifests()
    case 'observation_schema':
      return m.startup_phase_observation_schema()
    case 'observation_data':
      return m.startup_phase_observation_data()
    case 'schema_migrations':
      return m.startup_phase_schema_migrations()
    case 'migration_verify':
      return m.startup_phase_migration_verify()
    case 'gateway_initialize':
      return m.startup_phase_gateway_initialize()
    default:
      return progress.label
  }
}
