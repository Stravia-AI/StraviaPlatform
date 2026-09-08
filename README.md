<h1 align="center">Stravia AI Gateway</h1>

<p align="center">
  A local, self-hostable gateway for routing OpenAI, Anthropic, and Gemini clients to the model providers you choose.
</p>

<p align="center">
  <a href="README_CN.md">中文文档</a>
</p>

> **Project status:** Stravia is at `0.1.0` and under active development. Configuration and database compatibility may change before a stable release.

## Overview

Stravia runs between AI clients and upstream model providers. Clients keep speaking the protocol they already support; Stravia resolves a virtual model, selects an upstream backend, translates the request and response when necessary, and records the result locally.

```text
Claude Code · Codex CLI · Gemini CLI · OpenCode · SDKs
                            │
                            ▼
              Stravia unified listener :23471
                ├─ OpenAI-compatible API
                ├─ Anthropic Messages API
                ├─ Gemini GenerateContent API
                ├─ Admin API
                └─ WebUI
                            │
                            ▼
 OpenAI · Anthropic · Google · Vertex AI · DeepSeek · Ollama · …
```

The same Rust core powers two deployment modes:

- **Desktop:** a Tauri application for a local, managed gateway.
- **Server:** a standalone binary that serves the proxy API, Admin API, health probes, and an embedded WebUI from one listener.

## Current Capabilities

### Protocol gateway

| Client-facing protocol    | Endpoint                                               |
| ------------------------- | ------------------------------------------------------ |
| OpenAI Chat Completions   | `POST /v1/chat/completions`                            |
| Open Responses 2026-04-24 | `POST /v1/responses` (JSON, SSE, WebSocket)            |
| OpenAI Embeddings         | `POST /v1/embeddings`                                  |
| Anthropic Messages        | `POST /v1/messages`                                    |
| Gemini GenerateContent    | `POST /v1beta/models/{model}:generateContent`          |
| Gemini streaming          | `POST /v1beta/models/{model}:streamGenerateContent`    |

Stravia supports JSON, SSE, and Open Responses WebSocket delivery, cross-protocol tool calls, reasoning content, usage data, and same-protocol pass-through when an upstream requires no mutation.

Open Responses reasoning content streams with the rolling `response.reasoning_text.delta` / `response.reasoning_text.done` event names used by current clients; the reasoning item and dated `2026-04-24` semantics remain unchanged.

Hidden Platform Tool continuations are projected into client history with HTML-comment History Markers. OpenAI-compatible Chat Completions keep pre-answer Thinking in `reasoning_content`; public Open Responses summary deltas remain live when encrypted reasoning was not requested, while protected reasoning identified at item start also streams its public summary and keeps opaque bytes behind the Marker. After the first non-empty `content` delta, later Thinking is streamed in `content` as a Markdown blockquote Preview and all later Thinking and Platform Markers also use `content` so field aggregation preserves order. Plain-text clients may display those raw Marker comments. Open Responses, Anthropic Messages, and Gemini retain native ordered reasoning/thinking carriers; an ordering the selected protocol cannot represent fails explicitly rather than delaying ordinary Text.

Clients that resubmit full history must preserve History Markers and Projection Delimiters verbatim. Stravia removes display-only Preview bytes and restores authoritative Thinking, ToolCall, and ToolResult segments at their original positions. Removing a Marker or Delimiter is treated as an intentional history edit. With client streaming disabled, Stravia executes Platform-only hidden rounds before returning one semantically equivalent buffered projection. Live streams deliver and publish each Marker before starting its Platform Tool.

OpenAI direct and Codex OAuth generation Targets use the upstream Responses WebSocket transport for Chat Completions, Open Responses, Anthropic Messages, and Gemini requests, regardless of client streaming mode. Embeddings remain HTTP-only. After Hooks and protocol representability checks, Stravia may continue from the longest exact reusable canonical item prefix; Principal, exact Target, Provider account/configuration, resolved model, instructions, tools, reasoning, response format, and request controls must all match. A mismatch sends the full effective history instead of weakening request semantics.

`POST /v1/responses` uses Open Responses 2026-04-24 as its canonical baseline while accepting structurally safe rolling additive fields and hosted-tool declarations. Same-protocol Targets preserve that compatibility envelope; cross-protocol Targets may omit advisory fields and optional hosted tools, but never content or hard constraints. `POST /v1/responses/compact` is recognized but returns `unsupported_feature`, and background execution remains unsupported.

### Providers and model routing

Built-in provider metadata currently covers:

- OpenAI and the Codex OAuth channel
- Anthropic and the Claude Code OAuth channel
- Google Gemini and Vertex AI
- DeepSeek, Moonshot AI, Zhipu AI, Z.AI, MiniMax, xAI (API key and Grok OAuth), and NVIDIA
- OpenRouter, Ollama, and custom OpenAI-compatible endpoints

A client sends a **Model ID**. That value is the Route ID and is matched exactly, including letter case. A logical Model may also have an optional, non-unique display name; labels fall back to Model ID and never affect routing, authorization, or bindings. The matching Route can retain enabled and disabled Targets; disabled Targets keep their configuration without receiving traffic. Stravia first selects the highest eligible Target Priority group, then uses Traffic Equalization or Latency Preference within that group; conversation and cache affinity can preserve a previously successful enabled Target when applicable. Stravia refreshes its Provider Catalog from revisioned `models.stravia.cn` indexes: lightweight Provider and Canonical Model indexes update atomically, while Provider-scoped inventories load only when needed. Catalog-backed Providers use their scoped inventory; account-level discovery remains the source of callable IDs and only enriches exact matches without adding Catalog-only models.

Provider setup starts by choosing a complete provider/channel option. API-key and OAuth channels are separate options and cannot be converted into one another after creation. For Codex and Claude Code OAuth, desktop and loopback WebUI sessions receive the callback automatically; remote WebUI sessions ask for the full callback URL after browser sign-in. While authorization is pending, all three environments also allow pasting the full callback URL manually, even when the automatic listener is active. Grok OAuth uses xAI's device authorization flow: the WebUI opens the verification page, displays the user code when needed, and polls until authorization completes; no callback URL is required.

Codex Provider Model synchronization uses the current upstream client contract, so newly version-gated models become available after synchronization. Generation requests include the model and optional service-tier routing hint required by the Codex backend.

The WebUI keeps each resource on one editing surface. When adding or editing a logical Model, the Model ID combobox searches Canonical Models by name or ID and also accepts custom IDs while the catalog is unavailable; choosing a template copies its display name, and both values remain editable. Canonical Models remain searchable templates for manual Provider Models as well; selecting one never creates a Backend or persists a hidden binding. Saving a new Provider opens its detail page and starts Provider Model synchronization; the detail views separate connection settings, persisted Provider Model inventory, and Route references. Provider Model metadata is saved from its drawer, while Selection Policy applies immediately and controls only Effective Availability for new Target candidates. Existing Route Targets are never rewritten merely because a Provider Model becomes unavailable. Administrators can explicitly re-import a discovered Provider Model from its exact Provider Catalog Entry; ordinary synchronization never overwrites local metadata.

The available-model inventory has a separate **Model specification** column, shared with Target editing and model details. Specifications come from the saved, editable Provider Model snapshot—not tested capabilities, runtime defaults, or platform-added features. Limits retain exact decimal K/M values (1K = 1,000 tokens); full token counts are available by hover or keyboard focus. Input and output modalities remain separate, and feature declarations distinguish supported, unsupported, and not registered. The specification column filters by minimum context/output tokens, input/output modalities, and all five features. Every selected condition must match; unknown values cannot satisfy selected conditions. Filters combine with search, availability, source, and usage, and can be cleared together.

Desktop specification filters use the table's standard column filter menu: **Apply** commits the draft, **Clear** removes that column's filter, and closing without applying discards edits. On mobile, specification conditions are included in the existing **Filter models** drawer.

Overview recommends one next setup action based on loaded configuration, not request history. Connect a model service, search its inventory, and add the models you need using their upstream IDs; an exact existing Model ID adds the service to that model instead of creating another. Add actions stay visible, and successful additions offer Connect clients without leaving the inventory. Configured and enabled counts describe saved settings, not verified upstream connectivity.

The Connect clients page builds incremental Stravia provider patches from the selected API Key's authorized Routes. It reuses enabled, unexpired keys with access to an enabled model: one eligible key is selected automatically, while multiple candidates require a choice. Missing resources link to their existing editors; Continue setup preserves still-valid selections for that page task without storing secrets or workflow progress. Stravia Desktop prioritizes writing the patch to the Connect Client Global Config and also offers Copy; the standalone server only offers Copy. Success confirms copying or writing, not a client connection, and setup sends no automatic validation request. Apply never selects a current/default model or writes a fused provider/model key. Claude Code is the exception: it requires and merges the default, Haiku, Sonnet, and Opus model mappings, without changing `effortLevel` or `autoCompactWindow`.

The Route Builder is a full page. Selecting a Provider automatically loads its available Provider Models, while an explicit advanced path supports unverified custom upstream model IDs. Enabled Targets appear in descending priority layers and disabled standby Targets remain in a dock; the detail dialog edits first-token timeout and cooldown in seconds, retry budget, and Thinking Level mapping without exposing priority integers. The Route chooses Traffic Equalization or Latency Preference for Targets in the same layer. Deleting a Provider removes its Targets atomically, deletes Routes left empty, and keeps Routes that still have another Target.

### Web Search and MCP

Optional Web Search exposes one public `web_search` capability that returns a terminal sourced Search Report rather than a single search page. A successful result includes the answer, cited public HTTP(S) sources, limitations, completion state, usage, and a stable `turn_id`. Pass that ID as `previous_turn_id` to continue from the complete same-principal ancestor chain or to create an independent branch; Stravia never selects an implicit latest turn.

Configure one Search Backend in the WebUI. Local Search runs a bounded Agent over ordered internal Web Access Search and Fetch sources: the seeded in-process Local Provider, Exa, or Zhipu. Each Web Provider can independently use the Gateway proxy. Codex Agentic Search uses one exact compatible Codex OAuth Responses Provider/model binding and ignores the Local budget. There is no fallback between Local and Codex.

The in-process Local Provider's Search and Fetch use `wreq` and `wreq-util` for Chrome-style HTTP transport. Dynamic pages run in real Chrome/Chromium, started lazily in headless mode and controlled directly from Rust over CDP; no Moli runtime or Node/Bun sidecar is required. Both Desktop and Server require a resolvable Chrome/Chromium executable before Local Search or Fetch can be enabled, including ordinary HTTP access. Install the browser on the machine running Stravia and reopen the Local service editor to refresh detection, or set `STRAVIA_CHROME_PATH` to its executable and restart Stravia. Without a browser, the UI disables Local activation and the management API returns `WEB_ACCESS_BROWSER_REQUIRED` without saving the change. Previously saved Local selections remain removable but are unavailable at runtime; remote Exa and Zhipu services are unaffected. Stravia does not download a browser automatically.

Under **Web search → Search services → Local → Edit**, the **Browser executable** input is filled with the current configured or detected path. Desktop's **Browse** button opens the operating system's file picker; Server supports typing or changing the path on the server host. Selecting a file only changes the draft; click **Save service** to apply it. Clear the input and save to remove the manual override. Use an absolute executable path; on macOS, use the executable inside the `.app` bundle. The setting is saved locally for the running instance, not in the shared database, and survives restarts. A manual path takes precedence over `STRAVIA_CHROME_PATH`, followed by automatic detection; saving an unchanged detected path does not pin it. Invalid explicit paths do not fall back to another browser. Saved changes apply to subsequent Web Access requests without restarting; existing requests keep their captured selection. Checking the path does not launch a browser, and Desktop does not bundle one.

The renderer ports [OMP's browser patches](https://github.com/can1357/oh-my-pi/tree/daf07999c2fee9b22edc7bf8fea1fb6272e0df5e/packages/coding-agent/src/tools/puppeteer), including all 14 stealth scripts, UA metadata, isolated-world evaluation without `Runtime.enable`, and evaluation without injected source URLs. These are fingerprint mitigations, not a guarantee against bot detection. HTTP and browser paths retain the selected Gateway proxy snapshot, isolated cookie/profile ownership, and Fetch safety limits. Browser traffic passes through a checked egress proxy without TLS interception; certificate verification and Chrome's sandbox remain enabled.

The platform Web Search switch controls explicit access for every valid API key. Each key separately controls MCP access and Transparent Injection; injection only adds selected enabled capabilities to compatible requests and does not restrict explicit or MCP calls. MCP clients connect to `POST /mcp`, use `Authorization: Bearer <key>`, and discover `web_search` only when both MCP access and the platform capability are enabled. OpenAI Responses native web-search declarations and hidden tool continuations use the same Search contract.

Web Search and Web Access configuration are deployment-local and are not included in configuration export/import. Search Turns retain report metadata and cited URLs, not fetched page bodies or internal Agent transcripts.

### Media Understanding

Media Understanding exposes one `understand_media` capability for static JPEG, PNG, and WebP images. If a parent route has an image-capable Target, Stravia sends the original image natively. Otherwise, a tool-capable parent Model can call the configured hidden visual Model and receive a validated Media Report with source Artifact IDs and a branchable `turn_id`. Stravia does not fall back from a failed native-vision route to the hidden Model.

Enable the platform capability, select a logical Model, and choose its Thinking Level on the **Media Understanding** page. The selector only lists enabled Models where every Target explicitly advertises image input, and the Thinking Level selector only lists levels supported by every Target. All valid API keys can then call `understand_media` explicitly; MCP access and Transparent Injection remain independent per-key controls. Hidden calls consume the caller's quota without granting direct access to the selected Model. External image URLs are restricted to public HTTPS destinations and snapshotted before use. Preprocessing always creates a bounded lossy JPEG derivative, ignores ICC profiles, and may reduce exact-color or fine-text OCR accuracy.

### Local management

The SvelteKit WebUI manages:

- Providers, authentication, model discovery, and connectivity checks
- Virtual models and their upstream backends
- API keys with generated or custom editable secrets, model bindings, expiration, Principal Concurrency Limit, and execution permissions
- Request logs, latency and token usage statistics
- Provider-reported quotas, request allowances, and balances in an **Allowance overview** matrix with filters, a reset timeline, and current-window exhaustion forecasts backed by 30-minute samples; live reads retain three-minute caching, per-provider refresh, and the last successful result when a refresh fails
- Runtime settings
- Ready-to-copy integration examples for SDKs and coding tools

The interface supports English and Simplified Chinese, responsive navigation, and light, dark, or operating-system themes. On first use, a Simplified Chinese (`Hans`) client locale selects `zh-CN`; unsupported locales use English. Language can be switched without reloading from Login or **Settings → Appearance**, and each browser or desktop WebView remembers its own choice.

The management UI checks public GitHub Releases for optional updates. Stravia Desktop checks when the app starts and can download a signed Windows x86_64/ARM64 NSIS or Linux x86_64/ARM64 AppImage update only after the user asks; the standalone server reports the exact Release and never replaces its own executable. Successful checks are cached for 24 hours, failed automatic attempts are limited for one hour, and **Settings → Updates** can always check again. Update traffic follows the instance outbound proxy when enabled and otherwise connects directly to GitHub.

### Storage and deployment

- **SQLite** and **PostgreSQL** are selected during the first-run setup flow.
- The selected database connection is stored only in `server.toml`; database CLI options and environment-variable overrides are not supported.
- SQLx migrations preserve data from the currently supported schema and run before the normal Gateway becomes ready.
- `GET /healthz` is the liveness probe; `GET /readyz` returns unavailable while setup is incomplete or the Gateway cannot start.

PostgreSQL must already exist and be reachable by an account that can create and migrate Stravia's own tables; Stravia does not create the database or require `CREATEDB`. Incompatible older schemas fail explicitly rather than being deleted or rebuilt.

## Releases

Version tags publish Server archives and Desktop installers through [GitHub Releases](https://github.com/Stravia-AI/StraviaPlatform/releases), alongside a multi-architecture container image and Nix packages. Release outputs currently cover:

- Server: Linux and Windows on x86_64 and ARM64; Linux provides both GNU and musl archives.
- Desktop: signed Tauri updater artifacts and ordinary Linux AppImage or Windows NSIS downloads on x86_64 and ARM64.
- Container: `linux/amd64` and `linux/arm64` under `ghcr.io/stravia-ai/straviaplatform`.
- Nix: native `x86_64-linux` and `aarch64-linux` packages from the repository flake, published to the [`stravia-platform` Cachix cache](https://app.cachix.org/cache/stravia-platform).

macOS artifacts are not currently provided. Linux GNU Server archives and Desktop AppImages use Ubuntu 24.04 as their compatibility baseline; use a musl Server archive on older Linux distributions. Windows Desktop installers are updater-signed but not Authenticode-signed, so they may still trigger Microsoft Defender SmartScreen.

Every downloadable build asset is listed in `SHA256SUMS`. Desktop updater artifacts also have `.sig` files and a versioned `stravia-updater.json` manifest; the embedded updater public key verifies the selected package before installation. Verify `SHA256SUMS` before manually running a binary or installer:

```bash
sha256sum path/to/downloaded-asset
```

Compare the result with the matching line in `SHA256SUMS`. On Windows, use `Get-FileHash C:\path\to\downloaded-asset -Algorithm SHA256`.

Maintainers publish from `vMAJOR.MINOR.PATCH` or SemVer prerelease tags whose version matches `Cargo.toml`, `package.json`, and `tauri.conf.json`; the tagged commit must belong to `master`. Stable releases update the image tags for the full version, minor version, major version, and `latest`. Prereleases update only their full version tag. The first GHCR publication creates a private package; a maintainer must change its visibility to **Public** once in the package settings.

## Quick Start from Source

### Prerequisites

- Rust `1.98.1`
- Bun `1.4.0`
- [Task](https://taskfile.dev/) `3.52.0`
- uv `0.11.28` for Python E2E tests
- CMake and Clang/libclang for native HTTP dependencies; Windows builds also need NASM
- Chrome/Chromium for dynamic Local Search/Fetch and `task test:browser`
- Platform dependencies required by Tauri when building the desktop app

### Run the standalone server

```bash
# Development mode with the Vite WebUI
task dev:server

# Build the embedded WebUI and release server binary
task build:server

# macOS / Linux
./target/release/stravia-server

# Windows
.\target\release\stravia-server.exe
```

The server listens on `127.0.0.1:23471`. Debug builds use the repository-local `.stravia-dev/` data directory; release builds use `~/.stravia`. On first start there is no implicit database: the console prints a one-time setup token, and <http://127.0.0.1:23471/setup> uses it to select SQLite or PostgreSQL and create the single administrator. After setup, sign in with that username and password before configuring providers and model routes. The setup token is consumed by its first successful claim and is replaced if the unfinished process restarts.

### Run the server with Nix

```bash
# Current checkout
nix run .

# Tagged release; replace vX.Y.Z with the required release
nix run github:Stravia-AI/StraviaPlatform/vX.Y.Z
```

The flake supports `x86_64-linux` and `aarch64-linux`, builds the embedded WebUI and Server as one package, includes Chromium for dynamic Local Search/Fetch, and configures the public `stravia-platform` Cachix cache as a substituter. An explicit `STRAVIA_CHROME_PATH` overrides the bundled browser.

For NixOS, import the service module from the flake:

```nix
{
  inputs.stravia.url = "github:Stravia-AI/StraviaPlatform";

  outputs = { nixpkgs, stravia, ... }: {
    nixosConfigurations.gateway = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        stravia.nixosModules.default
        {
          services.stravia.enable = true;
        }
      ];
    };
  };
}
```

The service listens on `127.0.0.1:23471` by default, runs with a dynamic system user, and persists its data and `/var/lib/stravia/server.toml` under `/var/lib/stravia`. Set `services.stravia.host`, `port`, and `openFirewall` when exposing it. A non-loopback listener also requires `STRAVIA_PUBLIC_ORIGIN` to be the canonical HTTPS origin; place that non-database setting in `services.stravia.environmentFile` and terminate TLS at a reverse proxy. Database settings are never read from the environment. For an existing PostgreSQL deployment, write the `[database]` configuration shown below to `/var/lib/stravia/server.toml` before starting the upgraded service.

### Run the server with Docker

```bash
# Pull the latest stable multi-architecture image
docker pull ghcr.io/stravia-ai/straviaplatform:latest

docker run --rm \
  --publish 127.0.0.1:23471:23471 \
  --env STRAVIA_PUBLIC_ORIGIN=https://gateway.example.com \
  --mount source=stravia-data,target=/data \
  ghcr.io/stravia-ai/straviaplatform:latest
```

Use `docker build --tag stravia-server:local .` and replace the final image name with `stravia-server:local` to build from the current checkout. The image embeds the production WebUI, listens on `0.0.0.0:23471` inside the container, runs as a non-root user, and persists `server.toml` and SQLite data under `/data`. Put an HTTPS reverse proxy in front of the loopback-published port and set `STRAVIA_PUBLIC_ORIGIN` to that exact external origin; management cookies are Secure and unsafe management requests require the same origin plus Stravia's CSRF header. Do not expose the container port directly over HTTP. The built-in health check calls `GET /healthz`; readiness remains unavailable until setup and Gateway startup complete.

The image includes Chromium. Dynamic rendering requires the host/container policy to permit Chrome's sandbox and its Linux namespaces; Stravia does not fall back to `--no-sandbox` when startup is denied.

After creating a virtual model such as `my-model`, call it through any supported client protocol:

```bash
curl http://127.0.0.1:23471/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_PROXY_KEY" \
  -d '{
    "model": "my-model",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

The authorization header is required only when the selected model route is protected by an API key.

### Run the desktop app

```bash
# Development mode
task dev:desktop

# Production bundle
task build:desktop
```

Development builds keep server and desktop runtime state—including `gateway.db` and the desktop fixed-port store—under the repository-local, ignored `.stravia-dev/` directory. Release server builds use `~/.stravia`; release desktop builds continue to use the operating system application-data directory.

Desktop builds with the `desktop-e2e` feature use a separate, ignored `.stravia-desktop-e2e/` directory, including when built in debug mode. The `task test:e2e:desktop` workflow seeds a fake `9.9.9` update there to exercise download and installation without fetching or installing a real release; these fixtures must not enter normal development or production data.

The `task dev:server` and `task dev:desktop` workflows enable debug-only wire capture under `.scratch/wire-captures/`. Captures contain full request and response bodies even though sensitive header values are redacted. Keep them local and delete them after diagnosis.

The desktop process starts the same unified HTTP application locally on `127.0.0.1`. On first use it prefers the fixed default port `23471`; later launches prefer any fixed port saved under **Settings → Desktop**. If the preferred port cannot be bound, Stravia remains available on a temporary random port, reports the conflict on Overview, and lets you recheck or replace the fixed port without restarting. This desktop-local setting does not change the standalone server options below.

## Server Configuration

Common CLI options and environment variables:

| CLI option               | Environment variable           | Default      |
| ------------------------ | ------------------------------ | ------------ |
| `--host`                 | `STRAVIA_HOST`                 | `127.0.0.1`  |
| `--port`                 | `STRAVIA_PORT`                 | `23471`      |
| `--public-origin`        | `STRAVIA_PUBLIC_ORIGIN`        | derived for loopback; required HTTPS origin otherwise |
| `--config`               | —                              | `<data-dir>/server.toml` |
| `--data-dir`             | `STRAVIA_DATA_DIR`             | Debug: `.stravia-dev`; release: `~/.stravia` |
| `--log-level`            | `STRAVIA_LOG_LEVEL`            | `info`       |
| `--config-poll-interval` | `STRAVIA_CONFIG_POLL_INTERVAL` | `3` seconds  |
| `--wire-capture-dir`¹    | `STRAVIA_WIRE_CAPTURE_DIR`     | unset        |

`--config` selects the only database configuration source. `--data-dir` still locates runtime artifacts and supplies the default config path; it does not select or override the database. A missing config enters first-run setup. A malformed config, unreachable configured database, or incompatible schema is a startup error and never falls back to SQLite.

The setup flow atomically writes one of these forms:

```toml
[database]
backend = "sqlite"
path = "/var/lib/stravia/gateway.db"
```

The SQLite filename must be `gateway.db`. Relative paths, including the setup wizard's default `gateway.db`, resolve from the directory containing `server.toml`, not the process working directory. Setup saves the resolved absolute path; existing absolute paths are unchanged. With the default Debug configuration, this uses `<workspace>/.stravia-dev/gateway.db`.

For an already-created PostgreSQL database:

```toml
[database]
backend = "postgres"
url = "postgresql://stravia:replace-me@postgres.example.com:5432/stravia"
max_connections = 10
min_connections = 1
idle_timeout_seconds = 300
```

The three pool settings are optional. Protect `server.toml` because a PostgreSQL URL can contain credentials. The connection account needs permission to run Stravia's migrations in that database, but not permission to create a database. Existing PostgreSQL deployments must create this file with their current connection URL **before the first upgraded start**. Removing the old database environment variables without doing so intentionally enters setup; Stravia will not infer the old PostgreSQL database or silently choose SQLite.

On an unconfigured or configured-admin-free database, the console token is accepted only by `POST /api/v1/setup/claim`; the resulting `stravia_setup` HttpOnly, `SameSite=Strict` cookie (`Path=/api/v1`, and `Secure` for HTTPS) can call `/api/v1/setup/test` and `/api/v1/setup/complete`. Setup access cannot call management APIs and is closed when an administrator already exists. `GET /api/v1/auth/state` reports setup, availability, and current authentication without refreshing credentials. Normal Server authentication uses `/api/v1/auth/login`, `/api/v1/auth/refresh`, `/api/v1/auth/logout`, and `/api/v1/auth/credentials`. Access and refresh values remain in `HttpOnly`, `SameSite=Strict` cookies (`stravia_access` with `Path=/`, and `stravia_refresh` with `Path=/api/v1/auth`), not browser storage; HTTPS origins add `Secure`. Remote management requires an HTTPS canonical origin. The browser client sends `X-Stravia-CSRF: 1`, and Stravia rejects unsafe requests whose `Origin` differs from `--public-origin`.

To recover forgotten credentials, run the local interactive command against the same config:

```bash
./target/release/stravia-server --config /var/lib/stravia/server.toml recover-admin
```

The command prompts for the username and reads the new password plus confirmation without echoing it or accepting it as a command-line argument. It updates the existing single administrator in place and revokes every old management session; it does not delete business data or reopen database setup.

¹ Debug builds only. When enabled, Stravia writes one correlated JSONL file per request containing client and upstream requests/responses. Sensitive header values are replaced with `***`; bodies are recorded as readable UTF-8 text in the `body` field and can contain prompts, tool data, media references, and model output. Keep captures local and delete them after diagnosis. Release builds contain neither this option nor the capture implementation.

Replay the upstream response frames through the real protocol decoder:

```powershell
$env:STRAVIA_WIRE_REPLAY_FILE = ".scratch/wire-captures/req-....jsonl"
cargo test -p stravia-core replay_wire_capture_from_environment -- --ignored --nocapture
```

Set `--public-origin` to the trusted, externally reachable Gateway origin (for example, `https://gateway.example.com`) before binding to a non-loopback host. Stravia never trusts forwarding headers to derive the management origin or signed Artifact URLs. Terminate HTTPS at a reverse proxy and forward requests unchanged to the Stravia listener:

```bash
./target/release/stravia-server \
  --host 127.0.0.1 \
  --port 23471 \
  --public-origin https://gateway.example.com
```

## Development

```text
backend/crates/stravia-core/       Transport-independent gateway, protocols, providers, storage, and admin service
backend/crates/stravia-devtools/   Fixture recording and replay tools
backend/apps/stravia-server/       Standalone unified HTTP server
backend/apps/stravia-desktop/      Tauri desktop shell
frontend/stravia-webui/            SvelteKit management interface
tests/e2e/                         Python backend E2E suites and recorded protocol fixtures
```

Common commands:

| Command                  | Purpose                                                     |
| ------------------------ | ----------------------------------------------------------- |
| `task dev:web`           | Start the WebUI development server                          |
| `task dev:server`        | Start the Vite WebUI and standalone debug server            |
| `task dev:desktop`       | Start the Tauri desktop app in development mode             |
| `task check`             | Run WebUI checks, ESLint, Rust formatting, and Cargo checks |
| `task test`              | Run WebUI and supported Rust unit tests                     |
| `task test:browser`      | Run real headless Chrome regressions against local fixtures |
| `task test:e2e:web`      | Run Chromium WebUI E2E tests                                |
| `task test:e2e:desktop`  | Run the Windows Tauri/WebView2 smoke test                   |
| `DB_URL=… task test:e2e` | Run the full proxy, Admin, SQLite, and PostgreSQL E2E suite |

To check Google independently, run `cargo test --locked -p stravia-web-access live_google_returns_parsable_destination_urls -- --ignored --nocapture`. This contacts Google through the system proxy snapshot and verifies actual result titles and destination URLs; it is not part of the default test suite.

Backend Python tests use the locked `test` dependency group in `pyproject.toml`; Task invokes them through `uv run --locked`.
Debug server builds do not embed or serve WebUI assets. `task dev:server` starts the Vite development server alongside the backend; release server builds embed the WebUI.

`task dev:server` starts Vite first and passes its actual listening origin to the backend's `--public-origin`. If port `5173` is occupied, Vite automatically selects another port; open the exact **Local** URL printed in the terminal. Concurrent workspaces should use distinct `STRAVIA_PORT` values for their backend listeners; WebUI ports need not be fixed.

If you run `task dev:web` and the backend separately, pass the WebUI's actual origin to the backend, for example `cargo run -p stravia-server -- --public-origin http://localhost:5174`. `localhost` and `127.0.0.1` are different browser origins. After restarting an unfinished setup, use the new setup token printed by the new Server process.

## Documentation

- [Architecture](docs/design/architecture.md)
- [Database schema](docs/database/schema.md)

## License

Stravia is licensed under the [GNU Affero General Public License v3.0 only](LICENSE) (`AGPL-3.0-only`).
Separately licensed components and assets retain their own terms. The original `stravia-web-access` code is under `CC0-1.0`; its vendored OMP stealth scripts are under [MIT](backend/crates/stravia-web-access/src/browser/stealth/LICENSE), with that notice included in the compiled injection script. Bundled fonts retain their respective licenses.
