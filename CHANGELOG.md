# Changelog

## Unreleased

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
