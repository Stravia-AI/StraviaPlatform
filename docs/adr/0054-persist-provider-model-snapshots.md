# Persist Provider Model snapshots instead of overlays

Status: accepted

> “编辑即接管整条规格”与“显式重新导入整条 Provider Catalog Entry”已由 [ADR-0084](0084-pin-provider-model-fields-individually.md) 取代：人工修改按字段固定，可逐字段恢复同步，整条重新导入已删除。其余约定继续有效。

Stravia persists each Provider Model as the Provider instance’s editable metadata snapshot. Its explicit state distinguishes `unregistered`, `imported` with a source stamp, and `edited` with any known previous source. An ID-only discovery does not invent capabilities, modalities, or context limits. A later discovery may supply the first real specification while the snapshot remains unregistered; subsequent reconciliation keeps imported specifications current with their source. Editing transfers ownership of the complete specification to the administrator, not just the edited fields; normal reconciliation preserves that specification. Administrators can explicitly re-import one complete Provider Catalog Entry when they want current catalog values.

Missing modalities are the effective-value exception: administration, manual-model preparation, capability queries, and runtime consumers default to text input and text output. Explicit modality lists, including empty lists, take precedence. This fallback does not write a declaration into an existing stored snapshot or make ID-only discovery count as an imported specification; context, reasoning efforts, and pricing remain unknown.

Route output eligibility is an explicit exception to consumers using that effective fallback: it reads the raw persisted output declaration. Missing modalities, missing output declarations, explicit empty output lists, and declarations containing only other modalities grant neither text nor image Route eligibility. A preparation flow that actually saves text output is a real declaration and qualifies subject to the other rules; snapshot state names, display fallback, and input-image support do not establish output support. A Provider Model can declare both text and image output, while each Route selects only one purpose.

## Considered Options

A sparse overlay would keep unedited fields current but requires inheritance, tombstones, deep merge, conflict display, and two simultaneous facts in every caller. Unconditional full synchronization is simpler to read but silently overwrites local corrections. Freezing imported specifications would preserve old values even when the administrator has never edited them. A persisted snapshot that follows its source until editing makes the record the single fact consumed by administration and future cost calculation, while preserving administrator-owned specifications until explicit re-import.

## Consequences

Provider Model data is stored in the database with queryable projections and preserved full metadata. Reconciliation may be automatically scheduled or user-triggered and remains atomic. Effective Availability is separate from metadata: `auto` follows discovery and lifecycle, while force policies preserve administrator intent. When a Provider Model becomes unavailable, existing Route and Target configurations remain saved, but subsequent upstream sends are subject to the same availability check as new selection.

Output declarations may also drift after binding: retain the Target and expose its mismatch, but exclude it before upstream execution; other eligible Targets may continue. Repairing the saved declaration restores eligibility without rebuilding the binding. New bindings and Route-purpose changes must not introduce mismatches, including disabled Targets retained in a purpose-change submission. Availability overrides cannot bypass output eligibility. Image Routes additionally require the existing media adapter checks; declaration mismatch filtering does not silently skip other adapter incompatibilities. See [ADR-0061](0061-bind-media-generation-to-routes.md).

Snapshot state is not inferred from equality with default metadata. Manual writes mark the snapshot edited, including writes that happen to equal a previous default. Reconciliation still refreshes plugin-owned execution metadata and discovery/lifecycle facts, but uses expected revisions to reject stale writes rather than overwrite concurrent edits. Existing records migrate conservatively to edited snapshots with unknown provenance; schema migration does not rewrite their specification JSON.
