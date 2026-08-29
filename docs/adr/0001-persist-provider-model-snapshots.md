# Persist Provider Model snapshots instead of overlays

Status: accepted

Stravia persists each Provider Model as the Provider instance’s editable metadata snapshot. Provider discovery supplies model IDs and the Provider Catalog enriches new records, but later reconciliation only adds models, updates discovery/lifecycle availability, and disables missing or deprecated entries; it does not rewrite existing capability, limit, or cost metadata. Administrators can explicitly re-import one complete Provider Catalog Entry when they want current catalog values.

## Considered Options

A sparse overlay would keep unedited fields current but requires inheritance, tombstones, deep merge, conflict display, and two simultaneous facts in every caller. Continuous full synchronization is simpler to read but silently overwrites local corrections. A persisted snapshot makes the editable record the single fact consumed by administration and future cost calculation, at the cost of intentionally stale metadata until explicit re-import.

## Consequences

Provider Model data is stored in the database with queryable projections and preserved full metadata. Reconciliation is user-triggered and atomic. Effective Availability is separate from metadata: `auto` follows discovery and lifecycle, while force policies preserve administrator intent. Existing route targets continue to operate when a Provider Model becomes unavailable for new selection.
