# Stravia Repository Instructions

## Scope

This file applies to the entire repository. Keep project-wide guidance here; put subsystem-specific guidance in a closer `AGENTS.md` only when that subtree needs different commands or conventions.

Stravia is a local AI protocol gateway implemented as a Rust workspace with these main components:

- `stravia-core`: transport-agnostic gateway, protocol, provider, storage, and administration logic.
- `stravia-server`: standalone HTTP server.
- `stravia-desktop`: Tauri desktop shell around the same core behavior.
- `stravia-webui`: SvelteKit management interface used by the server and desktop app.

## Repository map

| Path | Responsibility |
|---|---|
| `backend/crates/stravia-core/` | Core gateway, protocol conversion, providers, storage, and admin services. |
| `backend/crates/stravia-devtools/` | Development CLI, including `stravia-tools`. |
| `backend/apps/stravia-server/` | Standalone server binary and HTTP transport. |
| `backend/apps/stravia-desktop/` | Tauri desktop application and desktop integration. |
| `frontend/stravia-webui/` | Svelte 5 / SvelteKit management console. |
| `tests/e2e/` | Python end-to-end tests for proxy, admin, and storage behavior. |
| `docs/design/` | Current design documents and architecture detail. |
| `docs/database/schema.md` | Human-readable database schema reference. |
| `docs/adr/` | Architectural decision records. |
| `deploy/schema/postgres.sql` | Generated final-state PostgreSQL reference schema. |
| `Taskfile.yml` | Canonical repository workflows. |
| `CONTEXT.md` | Domain vocabulary and current domain model. |

## Architecture boundaries

- Keep `stravia-core` independent of Tauri, Axum request types, and WebUI concerns. Transport layers adapt requests to core APIs.
- Put administration business rules in core services. Server routes and desktop commands should remain thin adapters.
- Keep protocol conversion boundaries explicit. Provider adapters must not absorb client-transport or UI behavior.
- Do not duplicate backend business rules in the WebUI. The frontend presents and invokes the admin surface.
- Storage behavior must remain aligned across SQLite and PostgreSQL unless a documented decision explicitly narrows support.
- Reuse existing modules and interfaces before adding a parallel implementation. Prefer a clean cutover over aliases or compatibility shims.

## Setup and commands

The pinned toolchains are Rust `1.97.1` (`rust-toolchain.toml`) and Bun `1.3.3` (`package.json`). Python E2E dependencies are locked in `uv.lock`.

Use `Taskfile.yml` entry points for repository workflows:

| Goal | Command |
|---|---|
| Install locked WebUI dependencies | `task install:web` |
| Run the WebUI | `task dev:web` |
| Run the standalone server | `task dev:server` |
| Run the desktop app | `task dev:desktop` |
| Build WebUI, server, or desktop | `task build:web`, `task build:server`, `task build:desktop` |
| Run static checks | `task check` |
| Run supported unit tests | `task test` |
| Run proxy E2E tests | `task test:e2e:proxy` |
| Run admin E2E tests | `task test:e2e:admin` |
| Run SQLite storage E2E tests | `task test:e2e:storage:sqlite` |
| Run PostgreSQL storage E2E tests | Set `DB_URL`, then run `task test:e2e:storage:postgres` |
| Run WebUI E2E tests | `task test:e2e:web` |
| Run the full backend E2E matrix | Set `DB_URL`, then run `task test:e2e` |
| Run the Windows desktop smoke test | `task test:e2e:desktop` |

Run the narrowest relevant check first, then expand according to risk:

- Rust core: `cargo test -p stravia-core <test-filter>`, then `cargo check -p stravia-core`.
- Standalone server: `cargo check -p stravia-server --no-default-features`.
- WebUI: `bun run --filter stravia-webui test:unit`, then `bun run check:web` and `bun run lint:web`.
- Repository-wide or public-contract changes: `task check` and `task test`.
- Documentation-only changes: verify every referenced path, command, and product statement; do not run unrelated builds.

## Change conventions

- Make the smallest coherent change that fixes the root cause. Do not mix unrelated cleanup into feature or bug-fix work.
- Follow the existing naming, error handling, module layout, and test patterns in the affected subsystem.
- Keep errors explicit. Do not swallow failures or hide them with sleeps, reduced timeouts, blind retries, or input-specific exceptions.
- Treat all external input as untrusted. Never log, commit, or expose API keys, tokens, cookies, private keys, or production connection strings.
- Reuse current dependencies. If a dependency change is required, update its lockfile through the owning package manager.
- Do not edit generated outputs directly, including `frontend/stravia-webui/dist/` and `frontend/stravia-webui/src/lib/paraglide/`.
- Comments should explain intent, external constraints, or non-obvious tradeoffs rather than restating code.
- Do not push, publish, deploy, merge, or call production services unless the user explicitly requests it and the impact is clear.

## Language and user-facing copy

- English is the canonical default for multilingual values in backend defaults, UI fallbacks, generated examples, and documentation-derived constants.
- Add localized alternatives explicitly. Review English and Chinese UI copy together for the same intent and level of clarity.
- Write copy around the user's goal, action, and observable result. Do not expose storage details, protocol limitations, or lifecycle terminology unless users need them to act or recover from an error.
- When public behavior or setup changes, update `README.md` and `README_CN.md` together. Keep design and schema documents synchronized with the implementation they describe.

## Testing expectations

- Bug fixes require a reproduction, a fix, and a regression check that fails on the original behavior.
- New tests must defend observable behavior, boundaries, invariants, transitions, precedence, or real errors. Avoid tests of source text or incidental implementation details.
- Keep tests deterministic and isolated. E2E tests must not call production services.
- Verify the changed product surface: exercise HTTP behavior for server changes, browser behavior for WebUI changes, and the actual desktop application for Tauri changes.
- Before finishing, ensure affected callers, tests, documentation, and generated reference artifacts are updated or intentionally unchanged.

## Agent skills

### Issue tracker

Issues and specs live as local Markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Domain docs

This repository uses a single-context layout: root `CONTEXT.md` and `docs/adr/`. See `docs/agents/domain.md`.

## Database changes

A database schema change includes edits to SQLx migrations under either of these directories:

- `backend/crates/stravia-core/migrations/sqlite/`
- `backend/crates/stravia-core/migrations/postgres/`

For every schema change:

1. Update both backends where the feature is supported.
2. Update `docs/database/schema.md` to describe the final table and column definitions.
3. Regenerate the PostgreSQL reference schema from migrations:

   ```bash
   stravia-tools dump-schema --backend postgres > deploy/schema/postgres.sql
   ```

4. Verify the relevant SQLite and PostgreSQL storage tests.

`deploy/schema/postgres.sql` is derived output for DBAs. Do not hand-edit its schema body; only its header comment may be edited directly.

## Completion checklist

- The implementation follows the architecture boundaries above and removes obsolete paths introduced by the change.
- The narrowest relevant checks pass; broader checks run when behavior or public contracts changed.
- User-facing behavior, both README languages, design docs, schema docs, and lockfiles are synchronized where applicable.
- Generated output is produced by its source tool rather than edited by hand.
- The final diff contains no unrelated changes, secrets, placeholders, or unfinished follow-up work.
- Report the exact commands run, their results, and any verification that could not be performed.
