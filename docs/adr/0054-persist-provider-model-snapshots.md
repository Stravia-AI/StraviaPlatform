# Persist Provider Model snapshots instead of overlays

Status: accepted

Stravia persists each Provider Model as the Provider instance’s editable metadata snapshot. Its explicit state distinguishes `unregistered`, `imported` with a source stamp, and `edited` with any known previous source. An ID-only discovery does not invent capabilities, modalities, or context limits. A later discovery may supply the first real specification only while the snapshot remains unregistered; normal reconciliation preserves imported and edited specifications. Administrators can explicitly re-import one complete Provider Catalog Entry when they want current catalog values.

## Considered Options

A sparse overlay would keep unedited fields current but requires inheritance, tombstones, deep merge, conflict display, and two simultaneous facts in every caller. Continuous full synchronization is simpler to read but silently overwrites local corrections. A persisted snapshot makes the editable record the single fact consumed by administration and future cost calculation, at the cost of intentionally stale metadata until explicit re-import.

## Consequences

Provider Model data is stored in the database with queryable projections and preserved full metadata. Reconciliation is user-triggered and atomic. Effective Availability is separate from metadata: `auto` follows discovery and lifecycle, while force policies preserve administrator intent. Existing route targets continue to operate when a Provider Model becomes unavailable for new selection.

Snapshot state is not inferred from equality with default metadata. Manual writes mark the snapshot edited, including writes that happen to equal a previous default. Reconciliation still refreshes plugin-owned execution metadata and discovery/lifecycle facts, but uses expected revisions to reject stale writes rather than overwrite concurrent edits. Existing records migrate conservatively to edited snapshots with unknown provenance; schema migration does not rewrite their specification JSON.
