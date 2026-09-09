# Changelog

## Unreleased

### Added

- Credential Protection now provides the live read-only rule catalog, interaction-grouped discoveries of actually new mappings, and a local matching tester with Unicode-aware positions. The tester does not save input, contact providers, query saved secrets, or change protection state. Discovery summaries remain secret-free, retain failed or cancelled activity, and distinguish incomplete observations.
- Advanced Features now includes default-off, instance-wide reversible credential redaction using bundled native Betterleaks rules, API Key-isolated persistent mappings, and transparent restoration in responses and tool arguments. Disabling protection retains restoration of existing valid references; restored secrets are permanently masked in response and tool-execution diagnostics.
- Request Records now presents Connect Client Interactions as a live causal forest with a chronological inspector, Rejected Requests, and point-in-time Debug Bundle downloads.
- Administrators can explicitly enable process-local Debug capture for newly admitted runs, with mandatory credential redaction, 64 MiB per-Run and 2 GiB retained limits, visible partial-capture reasons, and 60-second single-use bundle tickets.
- Standalone servers now offer a one-time-token setup wizard and an interactive `recover-admin` command that resets the existing administrator and revokes prior sessions without rebuilding business data.

### Changed

- **Breaking (Rust API):** Media Understanding, Web Search, and Credential Protection now live in independent compile-time crates. Canonical IR, Hook, Agent, Artifact, and history contracts move to `stravia-runtime-contract`; Rust callers use the owning crate rather than old core re-exports. Core retains host adapters, authorization, execution, and delivery ownership. HTTP/MCP contracts, configuration keys, persisted formats, and credential restoration/publication semantics are unchanged.
- **Breaking:** Remote context compaction is client-initiated passthrough, not a platform policy. Routes no longer expose `compaction_enabled` or `compaction_threshold`; migration 39 removes those columns while retaining native state records. Unknown Target capability is tried upstream, and compaction success or errors return without retries, Target switching, local summaries, or injected defaults.
- Web Access admission, Local Fetch, and browser outbound checks now share static URL and IP rules in `stravia-web-access`. Adapter checks now also reject hosts that become non-public IPs after trailing dots are removed, matching existing core admission. DNS responsibilities, proxy snapshots, redirect checks, and address pinning remain unchanged.
- Advanced Features save enable/disable switches immediately, without a separate save click. Media and search configuration drafts retain explicit submission and are not submitted or cleared by capability toggles. Failed immediate updates retain the saved state with local feedback.
- Local workspace tabs and model-service detail links share one segmented visual treatment and wrap on narrow screens. Form controls use consistent radii, input actions retain 40px hit areas, and resource lists omit redundant section introductions.
- Credential Protection uses roomier rule rows and removes the persistent saved-policy strip. Settings feedback appears only for loading, saving, or failures.
- Credential rule browsing now shows names and meaningful matching conditions without repeated IDs or targets. The inspector replaces raw condition JSON with keyword chips, exclusions, path restrictions, component requirements, and collapsible rule parameters; rule IDs remain searchable.
- Credential Protection now opens on a searchable, sortable rule table with pagination and a rule inspector. Hit records and matching tests have separate tabs; wide-screen testing places input beside results. Redundant implementation explanations are removed, while protection details and actionable failure feedback remain available.
- Shared data tables use compact headers with a subtle opaque background, aligned labels, and subdued inactive sort indicators that highlight on hover or keyboard focus. Active sorting remains prominent. Vertical scrollbars start below sticky headers, including grouped headers and filter rows.
- **Breaking (Rust API):** `preview_connect_client_apply` now accepts only the configuration input; directory environments remain exclusive to Desktop write planning.
- The former Reversible Redaction page and sidebar entry are named Credential Protection; the existing route, setting key, and restoration behavior remain unchanged. Retained pre-feature observations are marked incomplete rather than backfilled with invented discovery history.
- **Breaking (Rust API):** `Vendor` and `VendorExtension` replace separate `auth_headers` and `build_url` hooks with purpose-aware `construct_request`, which owns the URL and default authentication for inference and Models requests. Existing OAuth bindings and specialized inference signing remain authoritative.
- Generation Chain writes require an explicit final Model Leg Target or Hook source instead of recovering provenance from response metadata; history still commits only after complete client delivery.
- The WebUI now shares shadcn-svelte feedback, progress, disclosure, navigation and pagination controls, with reusable secret inputs, request recovery, model filters and metric loading states. Mobile observation details use a modal Sheet while desktop details remain a resizable overlay; update notifications use the existing persistent toast surface without changing skip policy.
- **Breaking (Rust API):** `AgentTool::execute` returns `AgentToolOutput` with explicit payload semantics instead of bare JSON. Tool-result semantics survive platform and Agent adapters, Hook reconstruction, and retained history. Generation Chain payload version 5 prevents legacy client metadata from impersonating trusted tool-text markers; ambiguous legacy tool arrays are rejected while reversible protection is enabled and left unchanged while it is off.
- **Breaking:** Interaction Observation replaces legacy flat request logs and debug-build wire capture. Migration 34 removes old request-log rows instead of backfilling them; analytics and Route scheduling now read Confirmed Upstream Usage from Model Turn and Target attempt observations.
- Debug defaults off after every process start, is snapshotted independently when each run is admitted, and retains Observation metadata and managed Trace segments for the configured request-record retention period (seven days by default).
- The latest Request Records page continuously accepts new activity after its fixed anchor; historical 24-hour pages keep fixed boundaries and preserve already-open root membership until an explicit refresh or migration.
- The in-process Local Provider's Search and Fetch now use `wreq` for HTTP and real headless Chrome/Chromium over native Rust CDP for dynamic pages, replacing Moli while preserving proxy snapshots, cookie isolation, and fetch safety limits. The renderer includes all 14 OMP stealth scripts and its applicable CDP fingerprint mitigations without disabling Chrome's sandbox. Docker and Nix packages include Chromium; other installations can set `STRAVIA_CHROME_PATH`.
- The pinned Rust toolchain and minimum workspace Rust version are now `1.98.1`.
- The WebUI now links service connection, visible model inventory actions, and client setup through lightweight task guidance, reuses eligible API Keys, and preserves selections when returning from resource management.
- Model lists distinguish display names, client Model IDs, enabled state, and associated services; modified model drafts now warn before navigation without changing priority lanes or immediately applied destination settings.
- Client setup distinguishes copied and written configuration from verified connections, and credential guidance no longer claims editable secrets are shown only once.
- **Breaking:** A single administrator account and revocable sessions replace the static Admin Token. Web sign-in uses HttpOnly cookies, 15-minute access JWTs, rotating refresh tokens, and a fixed seven-day session lifetime. Inference and MCP continue to use API Keys.
- **Breaking:** Server database connections now come only from `server.toml`, selected with `--config`. Existing deployments must move database CLI and environment settings into this file before upgrading; missing or invalid configuration never silently selects another database.
- Desktop management now requires the same session authentication as Server. Only the native application can obtain its in-memory session; closing to the tray preserves it, while exiting revokes it.

### Fixed

- Remote compaction retains upstream error details across HTTP, SSE, and client WebSocket delivery, including failures after visible output, without weakening ordinary generation error masking. Locked Python E2E workflows explicitly select the repository PyPI index, and the pinned Rust toolchain includes `rust-analyzer`.
- Inference execution keeps its full asynchronous run state off caller frames, preventing Windows stack overflow when native compaction and credential diagnostics are combined.
- Immediate Web Access switches retain their saved values after failed updates without clearing independent search drafts. Dark-theme switches show the active primary color instead of an overridden inactive track.
- Settings wait for real configuration before offering editable proxy and retention controls. Resource-detail load failures provide consistent retry and return actions instead of appearing as missing resources.
- Credential Protection uses the shared segmented Tabs style with a rule-count badge instead of a full-width underline. Its tab bar wraps on narrow screens without internal scrollbars or clipped labels.
- API-key-only connections now support unauthenticated services: an empty key no longer blocks model discovery or inference, and upstream requests omit default authentication. Supplied keys, required channel and structured credentials, and Stravia client authentication retain their existing behavior.
- Standalone server client-configuration previews no longer require `HOME`, `USERPROFILE`, or client-directory environment variables, including on NixOS services. Portable paths are used directly without rewriting serialized configuration; Desktop local-path validation remains unchanged.
- Model discovery now follows the Provider's saved proxy choice, reports invalid proxy configuration without bypassing it, and consistently applies Vendor Models authentication across protocol aliases. Native Gemini query credentials are URL-encoded; custom Models endpoints retain their declared authentication and selected URL.
- Model Turns complete restoration and required mapping publication before their unique successful terminal event, including ephemeral Agent execution. Cancellation and deadlines interrupt pending publication without revoking committed mappings; later stream events cannot reverse completion or falsify confirmed upstream usage.
- Model ID suggestions no longer select a catalog entry during input-method composition, preserving the administrator's display-name draft. Observation canvas selection updates retain measured node dimensions so returning from mobile details does not hide the originating node or lose focus.
- Completed HTTP streams no longer appear interrupted or lose their Request Records parent links when clients stop reading after the protocol terminal event; observation finalization now waits for the stream's delivery and Generation Chain commit result.
- Google Local Search now recognizes JavaScript gates containing result-heading templates and follows browser-side navigation without waiting on a superseded document. Browser sessions also retain cookies across temporary tabs within the same runtime.
- The Server development workflow now passes Vite's actual listening origin to the backend for setup and sign-in, including when Vite selects another port for concurrent workspaces.
- Relative SQLite paths now resolve from the directory containing `server.toml` throughout setup, startup, and local recovery, keeping the default Debug database in the workspace's `.stravia-dev` directory instead of creating a database in the process working directory.

## [0.1.6] - 2026-09-05

### Added

- The management UI now checks GitHub Releases for optional updates; Stravia Desktop can download, verify, and install signed Windows NSIS and Linux AppImage updates, while standalone servers only open the exact Release page.

### Fixed

- Codex OAuth model discovery now uses client version `0.153.0`, and generation sends the required model and service-tier routing hint, restoring discovery and invocation of version-gated models such as `gpt-6-astra`.

## [0.1.5] - 2026-09-04

### Fixed

- Streaming responses now preserve UTF-8 characters split across transport chunks and exclude tool output from History Marker continuation, preventing corrupted text and invalid follow-up context.

## [0.1.4] - 2026-09-04

### Added

- Stravia Desktop can now apply incremental provider configuration patches for authorized Routes to supported Connect Client global configuration files while preserving unrelated settings.

### Changed

- **Breaking:** Route Target configuration now uses enabled state, signed priority groups, First Token Timeout, per-Target retry budgets, cooldowns, and `traffic_equalization` or `latency_preference` scheduling. The migration keeps existing Targets enabled, resets their priority to `0`, and removes the legacy `weight` field and strategy names.
- Client projection now streams visible text immediately while preserving ordered Thinking and Platform Tool history markers across follow-up turns.
- The Route Builder now presents priority groups as lanes and keeps disabled Targets in a separate dock.

### Fixed

- Open Responses reasoning streams now use current rolling reasoning-text event names and complete reasoning summaries correctly, restoring thinking display in Oh My Pi.
- Usage charts now fill missing time buckets, and Zhipu weekly allowance windows are parsed correctly.
- YAML-based Connect Client configurations now preserve numeric setting types.

## [0.1.3] - 2026-09-02

### Added

- Web Access now provides a built-in Local Provider plus Exa and Zhipu adapters, with automatic migration away from Brave and Tavily.
- Provider allowance monitoring now includes a consolidated overview, status evaluation, sample persistence, and exhaustion forecasts.
- Client configuration generation now covers WorkBuddy, ZCode, and image-capable model inputs.
- Nix packaging and a NixOS service are available for both x86-64 and AArch64 Linux.

### Changed

- OpenAI-compatible streams now deliver text immediately, preserve public reasoning summaries, and project trailing reasoning without buffering an entire model response.
- Admin Route payloads now use required `model_id` and optional `display_name`; Route IDs are matched exactly, including letter case.

### Fixed

- HTTP/SSE continuation now replays full history once when an upstream rejects continuation before execution, including xAI Zero Data Retention responses.
- Provider deletion cleanup, concurrent SQLite route writes, prompt-cache key stability, and model selector behavior have been corrected.
