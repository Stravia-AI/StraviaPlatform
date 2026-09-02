# Changelog

## Unreleased

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
