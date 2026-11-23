---
status: accepted
---

# Persist Provider Model snapshots instead of overlays

Stravia persists each Provider Model as the Provider instance’s effective metadata snapshot. ID-only discovery does not invent capabilities, modalities, or limits, or erase a known source. [ADR-0084](0084-pin-provider-model-fields-individually.md) replaced whole-spec takeover with field pinning: ordinary reconciliation updates unpinned fields, and restoring a pinned field uses its saved source value. The merge happens at write time so every reader sees one effective specification. There is no whole-model reimport.

Missing modalities are the effective-value exception: administration, manual-model preparation, capability queries, and runtime consumers default to text input and text output. Explicit modality lists, including empty lists, take precedence. This fallback does not write a declaration into an existing stored snapshot or make ID-only discovery count as an imported specification; context, reasoning efforts, and pricing remain unknown.

Route output eligibility is an explicit exception to consumers using that effective fallback: it reads the raw persisted output declaration. Missing modalities, missing output declarations, explicit empty output lists, and declarations containing only other modalities grant neither text nor image Route eligibility. A preparation flow that actually saves text output is a real declaration and qualifies subject to the other rules; snapshot state names, display fallback, and input-image support do not establish output support. A Provider Model can declare both text and image output, while each Route selects only one purpose.

## Considered Options

A read-time overlay would make each caller resolve two facts, inheritance, and tombstones. Unconditional synchronization could erase local corrections; freezing the whole snapshot would strand unedited fields on stale metadata. The persisted snapshot preserves a single effective read path without either loss. See the [architecture design](../design/architecture.md) for snapshot states and write coordination.

## Consequences

Provider Model data is stored in the database with queryable projections and preserved full metadata. Reconciliation may be automatically scheduled or user-triggered and remains atomic. Effective Availability is separate from metadata: `auto` follows discovery and lifecycle, while force policies preserve administrator intent. When a Provider Model becomes unavailable, existing Route and Target configurations remain saved, but subsequent upstream sends are subject to the same availability check as new selection.

Output declarations may also drift after binding: retain the Target and expose its mismatch, but exclude it before upstream execution; other eligible Targets may continue. Repairing the saved declaration restores eligibility without rebuilding the binding. New bindings and Route-purpose changes must not introduce mismatches, including disabled Targets retained in a purpose-change submission. Availability overrides cannot bypass output eligibility. Image Routes additionally require the existing media adapter checks; declaration mismatch filtering does not silently skip other adapter incompatibilities. See [ADR-0061](0061-bind-media-generation-to-routes.md).

Pinning is driven by which fields the administrator submits, not by equality with defaults. Reconciliation still refreshes plugin-owned execution metadata and discovery/lifecycle facts. Expected revisions prevent stale writes from overwriting concurrent edits.
