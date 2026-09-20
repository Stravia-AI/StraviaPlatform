<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="frontend/stravia-webui/static/stravia-logo-reversed.svg">
  <source media="(prefers-color-scheme: light)" srcset="frontend/stravia-webui/static/stravia-logo.svg">
  <img alt="Stravia" src="frontend/stravia-webui/static/stravia-logo.svg" width="96">
</picture>

<h1>Stravia</h1>

**Local agent infrastructure — one endpoint for every AI client.**<br>
Unified model access, tools that run for you, and shared access control,<br>
history, and usage — desktop app or single binary.

[![Release](https://img.shields.io/github/v/release/Stravia-AI/StraviaPlatform)](https://github.com/Stravia-AI/StraviaPlatform/releases)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE)
[![CI](https://github.com/Stravia-AI/StraviaPlatform/actions/workflows/ci.yml/badge.svg)](https://github.com/Stravia-AI/StraviaPlatform/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.98.1-orange)](rust-toolchain.toml)

[Releases](https://github.com/Stravia-AI/StraviaPlatform/releases) · [Changelog](CHANGELOG.md) · [中文文档](README_CN.md)

</div>

![Request Records — every client request, traced live](docs/assets/request-records.png)

> **Status:** Stravia is pre-1.0 and under active development. Configuration and database compatibility may change before a stable release.

## What is Stravia

Stravia is local, self-hostable **agent infrastructure** for developers who use AI coding clients or build agent-powered apps. Your clients keep the protocol they already speak — OpenAI, Anthropic, or Gemini — and simply point at Stravia. Stravia decides which upstream service should answer and translates between protocols when needed.

On top of model access, the platform runs tools itself: built-in web search and media understanding execute inside Stravia and hand results back to the model — through ordinary model requests or MCP, no client plugins required. Access control, request history, usage, and diagnostics all live in the same place.

```text
Claude Code · Codex CLI · Gemini CLI · OpenCode · SDKs
                            │
                            ▼
              Stravia unified listener :23471
                ├─ OpenAI-compatible API
                ├─ Anthropic Messages API
                ├─ Gemini GenerateContent API
                ├─ MCP tools
                ├─ Platform tools and built-in agent execution
                ├─ Admin API
                └─ WebUI
                            │
                            ▼
 OpenAI · Anthropic · Google · Vertex AI · DeepSeek · Ollama · …
```

Agent behavior is defined and versioned by the platform — Stravia is not a user-defined agent or visual workflow builder.

## Why Stravia

- **Drop-in endpoint for AI coding clients** — point Claude Code, Codex CLI, Gemini CLI, or OpenCode at `127.0.0.1:23471` and keep working. Each client keeps its own protocol; Stravia handles translation, routing, and failover.
- **Tools that run for you** — built-in web search (embedded [Moli](https://github.com/Stravia-AI/moli-stealth) engine with V8 rendering — no Chrome install, no sidecar) and image understanding execute inside Stravia and hand results back to the model. Expose them over MCP or add them automatically to compatible requests.
- **Keys, spend, and request history in one place** — give each app its own key with model and concurrency limits, see the usage providers actually report, watch every request live, and download a full debug bundle when something goes wrong.
- **Local-first, one Rust core** — a desktop app or a headless server binary; SQLite by default, PostgreSQL for teams; local or S3 file storage. No cloud dependency.

## Quick Start

**Desktop** — download the Windows (NSIS) or Linux (AppImage) installer from [GitHub Releases](https://github.com/Stravia-AI/StraviaPlatform/releases) and open it. The full platform runs locally with an integrated management UI. macOS builds are not currently provided.

**Server** — one container:

```bash
docker run --rm \
  --publish 127.0.0.1:23471:23471 \
  --mount source=stravia-data,target=/data \
  ghcr.io/stravia-ai/straviaplatform:latest
```

or `nix run github:Stravia-AI/StraviaPlatform`, or a platform archive from Releases (verify against `SHA256SUMS`).

**First run (both):**

1. Open <http://127.0.0.1:23471/setup> and paste the one-time setup token printed to the console. Pick SQLite or PostgreSQL and create the administrator.
2. **Add a provider** — API key or OAuth channel (Codex, Claude Code, Grok device flow, Devin). Stravia syncs the available model inventory.
3. **Add a model** — select upstream model IDs; the Model ID is the route clients call.
4. **Create an API key**, then open **Connect clients** — Stravia generates a ready-to-apply provider patch for Claude Code, Codex CLI, Gemini CLI, or OpenCode; the desktop app can write the config for you.

Then call it through any supported protocol:

```bash
curl http://127.0.0.1:23471/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_PROXY_KEY" \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

## Features

### Unified model access

| Client-facing protocol    | Endpoint                                               |
| ------------------------- | ------------------------------------------------------ |
| OpenAI Chat Completions   | `POST /v1/chat/completions`                            |
| Open Responses 2026-04-24 | `POST /v1/responses` (JSON, SSE, WebSocket)            |
| OpenAI Embeddings         | `POST /v1/embeddings`                                  |
| Anthropic Messages        | `POST /v1/messages`                                    |
| Gemini GenerateContent    | `POST /v1beta/models/{model}:generateContent`          |
| Gemini streaming          | `POST /v1beta/models/{model}:streamGenerateContent`    |

Cross-protocol tool calls, reasoning, and usage reporting are preserved; requests that need no changes pass through as-is.

### Providers and model routing

OpenAI (incl. Codex OAuth) · Anthropic (incl. Claude Code OAuth) · Google Gemini + Vertex AI · Devin (OAuth) · DeepSeek · Moonshot AI · Zhipu AI · Z.AI · MiniMax · xAI (API key and Grok OAuth) · NVIDIA · OpenRouter · Ollama · custom OpenAI-compatible endpoints.

Devin preserves distinct completion reasons and signed or redacted thinking for compatible-source replay, including signatures received after the answer. Multi-turn replay retains images in their original user messages and tool results, and keeps tool calls adjacent to their results even when a client returns thinking after the call. Custom tool text is carried as a JSON `input` string rather than discarded. For Anthropic requests, an explicit `output_config.effort` takes precedence over `thinking.type` and its token budget.

Codex OAuth omits unsupported sampling and output controls (`temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `top_logprobs`, `truncation`, `max_output_tokens`, and `max_tool_calls`) from both HTTP and WebSocket requests, including HTTP fallback. Devin accepts zero `presence_penalty` and `frequency_penalty` as absent; nonzero penalties, `seed`, and `stop` remain explicit unsupported-parameter errors.

Responses streams close each indexed reasoning item when its authoritative completion arrives, retaining late signatures without deferring the item to the end of the response or emitting duplicate completion events. Retained upstream Responses WebSocket connections continue handling Ping and Close frames between requests. Peer closure or unexpected idle data retires the connection and its continuation affinity before reuse; healthy connections remain reusable.

Clients call a **Model ID** you define — map it to one or more upstreams in priority layers: requests go to the top layer first, balanced by traffic or preferring the fastest target, and a conversation sticks to what worked before. A built-in catalog keeps provider model lists up to date.

Retryable failures exhaust a configurable per-target budget before cooldown (defaults: 6 total attempts, then 120 seconds). After cooldown, one eligible request probes the target; a complete success restores normal routing, while a failed probe starts another cooldown without extra retries. Set cooldown to `0` to disable this cycle. The model editor refreshes destination status every 2 seconds while visible, showing cooling, waiting-to-retry, and retrying targets in amber.

![Editing a model — upstream targets in priority layers](docs/assets/model-routing.png)

### Platform tools and built-in agent runtime

- `StraviaRead` — one tool, one `path`: read files, webpages, `search://` questions, and images; Office documents (DOCX/XLSX/PPTX/DOC/XLS/PPT) read as extracted Markdown; long results page through automatically.
- **Web Search** — an agent loop searches and reads pages for the model, using the embedded Moli engine, Exa, or Zhipu — or a Codex search binding.
- **Media Understanding** — describe images and extract text (JPEG/PNG/WebP) or answer questions about Office documents with the vision model you choose.
- Expose everything over `POST /mcp`, or add it to compatible requests automatically. Loops run under hard time, turn, token, and tool budgets.

### Keys, usage, and request history

- API keys with custom secrets, model bindings, expiry, and per-key limits for concurrency, MCP access, and automatic tool injection.
- Request Records: watch every interaction live on a zoomable canvas — model calls, retries, and tool calls — with a separate failed-requests list and a per-conversation view.
- See the token usage and provider quotas that services actually report.
- Turn on Debug to capture HTTP/SSE/WebSocket traffic and download it as a debug bundle for the interaction you're inspecting; credentials are always redacted first.
- Transport failure diagnostics retain the failure stage and available underlying causes, with redaction and explicit length limits. Debug capture reassembles Command Code NDJSON records across network chunks, including split UTF-8, and marks incomplete tails rather than silently treating them as complete.

### Credential protection

Off by default — turn it on once and it covers every Stravia API key, with nothing to change in your clients.

- **Secrets stay on your machine** — Stravia recognizes API keys, tokens, and other credentials in messages, history, and tool inputs and outputs bound for a model, and swaps them for placeholders before the request goes out. All 1,475 detection rules are bundled and run locally — nothing is sent anywhere to be checked.
- **Replies still work end to end** — when a provider echoes a placeholder back, Stravia restores the real value in the answer and in tool calls, so tools can keep using the credential. The same secret keeps the same placeholder across conversations and restarts.
- **Fails safe, never silently** — if detection or storage breaks, the request fails instead of shipping your plaintext. Turning it off stops new replacements; placeholders already issued still restore until they expire.
- **Visible and testable** — browse the full rule list, see which credentials each request caught, and try your own text against the detector — test input stays on this instance and is never saved.

Good to know: it catches what its rules know — not every secret, and not personal or business data. It can over- or under-match and doesn't scan images or attachments. Tools receive the real credential when they run, so keep tool permissions and outbound limits in place — this lowers exposure, it isn't a lock.

### Storage and deployment

- SQLite or PostgreSQL, chosen at first-run setup; files in local storage or S3.
- Everything a deployment owns sits under one data directory, protected by an instance lock; a migration tool upgrades older layouts.
- One port serves the model APIs, MCP, the admin API, health checks, and the built-in management UI; deploys cleanly behind a reverse proxy.

## Deployment modes

|               | Desktop                                             | Server                                      |
| ------------- | --------------------------------------------------- | ------------------------------------------- |
| Form          | Tauri app with integrated management UI             | Single headless binary or container image   |
| Best for      | Individual developers; writes client config locally | Self-hosted and shared team deployments     |
| Storage       | Local SQLite                                        | SQLite or PostgreSQL                        |
| Get it        | Windows NSIS / Linux AppImage on Releases           | Archive · `ghcr.io` image · Nix flake       |

The same Rust core powers both; the management surface is the same WebUI.

## Documentation

- [Architecture](docs/design/architecture.md) and [design documents](docs/design/) · [ADRs](docs/adr/)
- [Database schema](docs/database/schema.md) · [Changelog](CHANGELOG.md)

## Development

Rust `1.98.1` · Bun `1.4.0` · Task `3.52.0` · uv for Python E2E.

| Command                  | Purpose                                                     |
| ------------------------ | ----------------------------------------------------------- |
| `task dev:server`        | Start the Vite WebUI and standalone debug server            |
| `task dev:desktop`       | Start the Tauri desktop app in development mode             |
| `task check`             | Run WebUI checks, ESLint, Rust formatting, and Cargo checks |
| `task test`              | Run WebUI and supported Rust unit tests                     |
| `DB_URL=… task test:e2e` | Run the full proxy, Admin, SQLite, and PostgreSQL E2E suite |

## License

Stravia is licensed under the [GNU Affero General Public License v3.0 only](LICENSE) (`AGPL-3.0-only`).
Separately licensed components retain their own terms: `stravia-web-access` is `CC0-1.0 AND MIT` ([Cargo.toml](backend/crates/stravia-web-access/Cargo.toml)), and the embedded [Moli engine](https://github.com/Stravia-AI/moli-stealth) is a separate repository under its own license. Bundled fonts retain their respective licenses.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=Stravia-AI/StraviaPlatform&type=Date)](https://star-history.com/#Stravia-AI/StraviaPlatform&Date)
