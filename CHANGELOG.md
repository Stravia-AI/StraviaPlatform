# Changelog

## Unreleased

### Added

- Advanced Features now includes default-off, instance-wide reversible credential redaction using bundled native Betterleaks rules, API Key-isolated persistent mappings, and transparent restoration in responses and tool arguments. Disabling protection retains restoration of existing valid references; restored secrets are permanently masked in response and tool-execution diagnostics.
- Request Records now presents Connect Client Interactions as a live causal forest with a chronological inspector, Rejected Requests, and point-in-time Debug Bundle downloads.
- Administrators can explicitly enable process-local Debug capture for newly admitted runs, with mandatory credential redaction, 64 MiB per-Run and 2 GiB retained limits, visible partial-capture reasons, and 60-second single-use bundle tickets.
- Standalone servers now offer a one-time-token setup wizard and an interactive `recover-admin` command that resets the existing administrator and revokes prior sessions without rebuilding business data.

### Changed

- **Breaking (Rust API):** `Vendor` and `VendorExtension` replace separate `auth_headers` and `build_url` hooks with purpose-aware `construct_request`, which owns the URL and default authentication for inference and Models requests. Existing OAuth bindings and specialized inference signing remain authoritative.
- Generation Chain writes require an explicit final Model Leg Target or Hook source instead of recovering provenance from response metadata; history still commits only after complete client delivery.
- The WebUI now shares shadcn-svelte feedback, progress, disclosure, navigation and pagination controls, with reusable secret inputs, request recovery, model filters and metric loading states. Mobile observation details use a modal Sheet while desktop details remain a resizable overlay; update notifications use the existing persistent toast surface without changing skip policy.
- **Breaking (Rust API):** `AgentTool::execute` returns `AgentToolOutput` with explicit payload semantics instead of bare JSON. Tool-result semantics survive platform and Agent adapters, Hook reconstruction, and retained history. Generation Chain payload version 5 prevents legacy client metadata from impersonating trusted tool-text markers; ambiguous legacy tool arrays are rejected while reversible protection is enabled and left unchanged while it is off.
- **Breaking:** Interaction Observation replaces legacy flat request logs and debug-build wire capture. Migration 34 removes old request-log rows instead of backfilling them; analytics and Route scheduling now read Confirmed Upstream Usage from Model Turn and Target attempt observations.
- Debug defaults off after every process start, is snapshotted independently when each run is admitted, and retains Observation metadata and managed Trace segments for the configured request-record retention period (seven days by default).
- The latest Request Records page continuously accepts new activity after its fixed anchor; historical 24-hour pages keep fixed boundaries and preserve already-open root membership until an explicit refresh or migration.
- The WebUI now links service connection, visible model inventory actions, and client setup through lightweight task guidance, reuses eligible API Keys, and preserves selections when returning from resource management.
- Model lists distinguish display names, client Model IDs, enabled state, and associated services; modified model drafts now warn before navigation without changing priority lanes or immediately applied destination settings.
- Client setup distinguishes copied and written configuration from verified connections, and credential guidance no longer claims editable secrets are shown only once.
- **Breaking:** A single administrator account and revocable sessions replace the static Admin Token. Web sign-in uses HttpOnly cookies, 15-minute access JWTs, rotating refresh tokens, and a fixed seven-day session lifetime. Inference and MCP continue to use API Keys.
- **Breaking:** Server database connections now come only from `server.toml`, selected with `--config`. Existing deployments must move database CLI and environment settings into this file before upgrading; missing or invalid configuration never silently selects another database.
- Desktop management now requires the same session authentication as Server. Only the native application can obtain its in-memory session; closing to the tray preserves it, while exiting revokes it.

### Fixed

- Model discovery now follows the Provider's saved proxy choice, reports invalid proxy configuration without bypassing it, and consistently applies Vendor Models authentication across protocol aliases. Native Gemini query credentials are URL-encoded; custom Models endpoints retain their declared authentication and selected URL.
- Model Turns complete restoration and required mapping publication before their unique successful terminal event, including ephemeral Agent execution. Cancellation and deadlines interrupt pending publication without revoking committed mappings; later stream events cannot reverse completion or falsify confirmed upstream usage.
- Model ID suggestions no longer select a catalog entry during input-method composition, preserving the administrator's display-name draft. Observation canvas selection updates retain measured node dimensions so returning from mobile details does not hide the originating node or lose focus.
- Completed HTTP streams no longer appear interrupted or lose their Request Records parent links when clients stop reading after the protocol terminal event; observation finalization now waits for the stream's delivery and Generation Chain commit result.
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
