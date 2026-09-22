# Changelog

## Unreleased

### Added

- Vendor Plugins provide self-contained Wasmtime Components with a versioned WIT contract, shared SDK, and build-time-pinned codecs. Exactly five bundled Vendor packages use the same runtime as local imports: one base fallback retains every existing integration except Codex, Grok, Command Code, and Devin, whose four dedicated packages completely own their supplier identities without per-channel, per-capability, or failure fallback. Server and Desktop share plugin import, update previews, declarative configuration, standard OAuth, and explicit network-origin review; installation does not require a marketplace or online dependencies.
- Media Generation exposes one `generate` tool through platform-managed model calls and MCP. Image generation and editing use saved Routes backed by capable Vendor plugins—including bundled Codex OAuth—plus safe reference ingestion and Principal-scoped Artifacts with actual image metadata. The configuration-only Advanced Features page supports English/Chinese and light/dark themes. Per-key automatic injection is independent and defaults off on SQLite and PostgreSQL upgrades; retries reuse existing Route policy and may consume quota more than once.

### Changed

- **Breaking:** Public resource references now use `stravia://artifacts/<artifact-id>`, `stravia://turns/<turn-id>`, and `stravia://turns/<turn-id>/sources/<ordinal>`. `StraviaRead`, Web Search, Media Understanding, Media Generation, bridge markers, and reports use `path` / `previous_path` and complete URI citations; `sa:`, `[sc:...]`, `[sm:...]`, `[st:...]`, raw Turn IDs, and the Open Responses `stravia:media_result` extension are removed without compatibility aliases. Persisted Artifact IDs, TurnNode IDs, upload/download grants, tool call IDs, and existing history bytes are not rewritten; callers must migrate to the new public shapes, and old history references may fail rather than being silently rewritten. Standard Responses `id` / `previous_response_id` and agent-result protocol `id` remain unchanged.
- Provider connections now keep their saved UUID separate from the supplier identity used to select a Vendor profile and channel. Plugins own upstream inference, authentication, model discovery, allowance normalization, refresh, full-search, and image-generation behavior while the host retains scheduling, network authorization, cancellation, retries, history, and Artifact ownership. External search supports Provider-only Route Targets and does not accept continuation; Local search retains its existing continuation contract.
- Existing cloud connection settings migrate out of credential storage into non-secret Vendor Options on both SQLite and PostgreSQL. Explicit Vendor Options take precedence, while saved secrets and connection identities remain intact.
- The base component covers the complete bundled Provider Catalog through a build-generated static registry. Compatible brands retain their adapter-family behavior without inheriting unrelated OAuth channels. Native account inventories and channel-specific curated lists remain authoritative despite saved catalog-origin markers; directory-backed profiles keep their scoped inventory, and explicit static lists override either source.
- Plugin Components are immutable, content-addressed instance files under `plugins/artifacts/`; SQL keeps installation metadata and business/private state, never Wasm bytes. Data migration copies these files with the instance root, and complete backups pair instance files with the database—a remote PostgreSQL backup alone is insufficient.
- Compatible plugin updates keep active calls on their original version. Bundled plugins upgrade forward automatically unless locally replaced. Data-incompatible updates require explicit discard confirmation, cancel that Vendor's active work, and reject late writes; selective resets retain Provider identities, Route/Target bindings, history, and confirmed usage. Restoring a bundled version exposes downgrade and compatibility consequences before confirmation; unavailable bundled installations can be restored directly without first importing a replacement.
- History storage now deduplicates repeated instructions, tool definitions, and response profiles within each Principal, without compression or changing replay semantics. SQLite and PostgreSQL keep explicit content references with branch-safe retention. Debug segments independently deduplicate metadata and payloads; exports restore complete records. `stravia-tools migrate-data --optimize-storage` verifies and optimizes an offline SQLite destination copy, preserving source data for rollback and reclaiming free pages only in the copy. Older binaries cannot read the new storage representation.
- Wire Debug captures four raw directions at application transport boundaries. Only HTTP `Authorization` header values are masked before queueing; other credentials, content, and media may remain recoverable, so bundles remain sensitive diagnostic data. Historical Trace records remain unchanged.

### Removed

- Removed Nix flake packaging, the NixOS service module, and their release/cache jobs. Server deployments use release archives or the container image.

### Fixed

- An empty hidden-round ledger no longer turns confirmed response usage into unknown usage. Streaming and buffered Responses preserve known zero values without inventing values for unreported usage.
- Observation attribution reuses ancestor windows from the same Generation Chain traversal instead of repeatedly restoring overlapping histories for each retained-tail candidate. Confirmed-parent diagnostics, ambiguity checks, Principal isolation, and resource limits remain unchanged.
- Attachment normalization preserves Artifact error categories: invalid input stays a client error, while storage failures return sanitized server errors. SQLite Artifact sweeping releases the database writer during object I/O and keeps its object lock until in-flight deletion settles, including caller cancellation; re-upload repairs expired objects before making them readable again.
- Observation reserves queue space for lifecycle records, preserves completion ordering, and reconciles missing activity terminals only after the last observer is released, without ending live background tools or discarding confirmed usage. Missing parent observations and failed Generation history commits remain explicit diagnostics. Completed interactions display earlier request failures separately, and delivered non-text output no longer appears undelivered.
- Observation now coalesces adjacent same-scope text before enqueueing, preventing fine-grained streaming bursts from displacing request admission and completion records. Existing missing observations and split interaction links are not reconstructed.
- Redacted Responses continuations selected by Route ID retain native continuation when the upstream reports effective generation or reasoning defaults.
- Local response-delivery deadlines no longer count as upstream failures or put a Target into cooldown.
- Cross-protocol Responses continuations retain the selected upstream response ID without mistaking client projection for a Hook rewrite. Deadline failures preserve their typed terminal error under stream backpressure, and independent search/image calls retain confirmed or unknown usage in request statistics.
- Continuation and authentication recovery retain the original Wasm component and cancellation generation across compatible updates. OAuth token rotation preserves account affinity, while reconnecting invalidates prior native state. Real upstream 401 responses can use the host's bounded authentication recovery, including GitLab auxiliary tokens; 403 responses are not refreshed or replayed.
- Explicit model-directory endpoints preserve supplier authentication and their configured paths. Plugin-update cancellation remains cancellation in external search and image-generation results instead of becoming an upstream failure.
- Route editing distinguishes unavailable capability metadata from confirmed unsupported capabilities and offers retry. Plugin availability is localized, and compatible-update previews no longer claim that active authorization sessions will be cancelled.
- Target cooldown now uses one consecutive upstream-failure count across internal retries, transport recovery, and later requests. The default retry budget of 5 starts cooldown on the sixth failure rather than the first committed-stream disconnect; complete success resets the count. Cancellation and local preparation failures do not count, half-open probes remain single-attempt, and output already delivered to clients is never transparently replayed.
- Immediate continuations now wait for matching in-process Generation commits before discovering or materializing their parent, preventing stale-parent Interaction branches without serializing unrelated requests. Failed or cancelled writes never publish resumable history, and observation backpressure cannot hold execution.
- Responses stream errors preserve canonical failure categories from upstream decoding through public delivery. Transport failures use the interoperable `server_error` code; permanent request and quota errors no longer inherit a retryable generic stream category. Nested dated error envelopes, diagnostic masking, terminal ordering, and the no-replay boundary after client output commit remain intact.
- Desktop startup failures now retain an independent recovery window with safe diagnostics, bounded startup logs, restart, and exit instead of disappearing. Optional desktop integrations degrade to warnings; failed silent launches become visible, failed-window close exits, and native dialogs cover window-creation errors without resetting or repairing stored data.
- HTTP and Responses WebSocket failures retain redacted, length-bounded cause chains and transport-stage diagnostics, including observed response state, known HTTP status, and WebSocket close codes, without changing retries or completion semantics.
- Wire Debug capture preserves each observed application-protocol chunk, including malformed, incomplete, and non-UTF-8 bytes, without JSON/SSE/NDJSON reassembly or media externalization; complete and partial Trace status remains tied to capture/storage boundaries rather than protocol parseability. Plugin HTTP capture keeps concurrent responses isolated even at the same URL, so initialization and auxiliary calls cannot consume inference records.
- Responses streams now close indexed reasoning items on their authoritative `ItemDone`, preserving late signatures and preventing already-completed thinking from being replayed after subsequent tool calls by OMP. Terminal responses retain the same items without duplicate completion events.
- Devin multi-turn tool continuations preserve signed thinking without separating tool calls from their matching results, fixing `invalid_argument` failures with OMP Responses history. Historical user images and images returned by client tools remain attached to their original messages instead of being replaced or silently discarded.
- Devin request encoding groups Responses assistant text, signed reasoning, and parallel tool calls into native prompts without combining independent historical signatures. Within one Devin response, interleaved text, thinking, and signature deltas retain their response-wide identity; replay omits response `output_id` from native history prompts, matching the native CLI. Top-level tool descriptions use numbered sentences and list items while preserving fenced code and JSON paragraphs, followed by XML escaping.

## [0.3.0] - 2026-09-19

### Added

- Windows NSIS installers now include English and Simplified Chinese. The first interactive install offers a language choice defaulting to the Windows UI language when it matches; later installs and uninstalls reuse the saved choice. Passive updater installs still skip wizard pages and reuse a saved installer language.
- `StraviaRead` now extracts Office documents — docx, xlsx, pptx, doc, xls, and ppt — to paginated Markdown snapshots, with ZIP central-directory preflight, container magic validation, and encrypted-CFB identification; all CPU work runs on the blocking pool under cancellation and deadlines. Embedded images deduplicate by content bytes into Artifacts (decodable ones normalized to JPEG, originals retained with a recorded limitation), and image markers in the Markdown resolve to `sa:` links. `?question=` routes documents through Media Understanding, `?download=1` downloads without parsing, and `?raw` is explicitly rejected.
- Devin Provider support uses the `devin-connect` Connect-RPC protocol with binary Protobuf streaming, an OAuth PKCE authentication driver, model discovery, allowance monitoring, and usage parsing that reports cached and reasoning tokens. Devin model families aggregate related selectors into one logical model record; request content redaction neutralizes competing-vendor identity fingerprints, and selector matching covers speed lanes and the 1M-context tier.
- Command Code Provider support (`command-code`) ships as a built-in catalog Provider with NDJSON streaming, CLI envelope encoding, and allowance monitoring that chains whoami, credits, subscriptions, and summary calls to report window quotas, point balances, and billing periods.
- Vendor Options let Providers declare non-secret behavior toggles such as zero data retention; the WebUI provider editor renders them dynamically and persists them per Provider.
- Routes accept a default thinking level applied when the client does not specify a reasoning instruction, clamped at runtime to the Route's Supported Thinking Levels. The model editor exposes the default with validation, and unrepresentable thinking controls degrade to Hidden on read instead of guessing wire shapes — explicit submissions of unrepresentable controls still fail with `THINKING_CONTROL_UNREPRESENTABLE`.
- Usage analytics gains a `/stats/series` endpoint with caller-selected bucket size and timezone offset aligned to local wall-clock time, a token-activity heatmap that adapts columns to container width, per-Provider average output speed, and pie charts for model distribution and input/output/cache-read/cache-write token composition.
- Provider allowances load tiered and asynchronously: the list returns targets immediately while background fetches resolve each snapshot, concurrent requests for one Provider coalesce, and the WebUI renders the shell first with per-Provider loading and error states before the aggregate view resolves.
- DeepSeek balance reporting now surfaces CNY and USD accounts as separate currency-keyed rows, filters exhausted balances, and retains one USD record when the account is fully depleted.
- The desktop client's settings are consolidated under Client Settings: a LAN-binding toggle switches the gateway listener between `127.0.0.1` and `0.0.0.0` with a retry loop that shrinks the rebind window, silent start and launch-at-login preferences control whether the main window appears on startup, and the external address display offers one-click URL copy. The tray menu follows the WebUI language without a startup flicker, drops the Copy Proxy URL entry, and left-click now only raises the main window.
- The observation inspector renders the diagnostic execution flow as a flattened, numbered timeline (R1, R2, …) with "continues from" links that jump to the parent Run, inter-Run gap rows distinguishing client-tool execution from idle waits, and process events grouped into collapsible counts. Scrolling to the top of an Interaction conversation auto-loads earlier events with the scroll anchor preserved.
- Interaction chains now hide Runs that never delivered client-visible output and ended in failure; hidden nodes appear through the Failed Requests list or deep links, and Interactions containing failed Runs show a diamond failure state at their terminal node. A minimum-chain-token slider filters chains below a threshold on the logs page, whose default time window is now ten minutes.
- Debug Trace data can be cleared through an admin endpoint and UI action; per-Run and global Debug Trace capacity caps are removed in favor of tracked retained bytes, and clearing marks active Traces partial via `debug_data_cleared`.
- Provider Model records match model IDs leniently — case-insensitive on the rightmost `/`-separated segment — with ambiguity rejected, and unmatched models receive conservative `bare` metadata (256K context, text modality, reasoning and tool calling) that stays upgradeable to registered specifications on later syncs.
- The WebUI marks Media Understanding, Web Search, and Artifacts features with a Beta badge.
- Browser-search profiles and HTTP-search cookies persist local search identities; Google search shares the browser profile and gates preflights on valid cookies, while fetch keeps its own isolated cookie store.

### Changed

- **Breaking:** Server and Desktop now keep managed local state beneath one resolved data root: SQLite at `db/gateway.db`, diagnostics under `diagnostics/`, rebuildable catalogs under `cache/`, and browser/host state under `state/`. SQLite configuration no longer accepts a separate path; `--config` does not redirect the data root. Pre-0.3.0 database layouts are not auto-upgraded — the server refuses to start on a legacy layout, and operators must stop every host using the source (including any external SQLite root) and run `stravia-tools migrate-data --from <old-data-root> --to <new-data-root>` (optionally `--config <legacy server.toml>` and `--webview-from <legacy WebView dir>`), which plans a verified copy by default and publishes it with `--apply --source-stopped` without deleting source data. Desktop restart and autostart retain the selected root.
- **Breaking:** Artifact creation returns upload-session credentials only; the final Artifact ID is returned on completion. Identical complete bytes and exact MIME under the same Principal now keep one identity across ingestion paths, multipart boundaries, concurrent uploads, and restarts. Re-uploading never shortens retention and can retain expired content again after full validation. Legacy random IDs and stored histories are not rewritten.
- **Breaking:** `StraviaRead` now accepts only `path`: `search://` replaces `query://`, search filters/continuation move into the search query, and resource options use `#stravia?`. Stravia-managed domain blacklists are removed; allowed domains constrain final report sources. Owned and public images default to understanding, HTML/text default to readable content, and explicit `download=1` avoids model execution. Strict raw decoding, line selections, and immutable 32 KiB/200-line text pages support complete long-line continuation. Long search/media answers paginate only their delivery copy; full history remains intact. Local Search uses Revision 3 and rejects incompatible old continuations. Empty text Artifacts are supported without changing the database schema.
- **Breaking:** Reversible redaction markers now use `<!-- stravia-redaction-marker:rm_<32 lowercase hex> -->` with inline-path and streaming restoration. Migration 46 converts valid legacy mappings, preserving identifiers, credentials, and lifecycle state; the old `~stravia-secret:…~` marker no longer parses or restores, so clients and sessions depending on it must start over.
- **Breaking:** Platform identities now use a 28-character random ID and 55-character SHA-256 base-26 encoding; legacy Artifact URLs, History Markers, redaction references, media and search citations, and cross-protocol tool-call IDs no longer parse. Old databases and history are not migrated or rewritten — callers establish sessions against a fresh database and the new protocol shapes.
- **Breaking:** The batch `POST /provider-allowances/refresh` endpoint is removed. Callers list allowance targets via `GET /provider-allowances` and fetch each snapshot via `GET /provider-allowances/{provider_id}`; `POST /provider-allowances/{provider_id}/refresh` remains for single-Provider refresh.
- Migration 44 allows multiple Media Understanding sources to share one JPEG derivative, including a source mapping to itself, while preserving existing mappings and cascade cleanup. Reports cite only declared current/ancestor sources with a displayed derivative; failed or losing mapping writes never delete shared content.
- The hourly usage statistics endpoint is replaced by `/stats/series` with `bucket` (seconds) and `tz_offset` parameters; reported usage keeps net input (cache reads deducted per attempt) and output (reasoning included) semantics across admin queries, projections, and statistics.
- Desktop no longer exposes provider-allowance commands (`list_provider_allowances`, `get_provider_allowance`, `refresh_provider_allowance`) or their capabilities; allowance monitoring remains available through the admin API and WebUI.
- Observation canvases no longer mount every new card at the origin for measurement. Shared node and handle geometry enables viewport culling before the first mount; unchanged nodes retain their identity, and long-chain ancestry checks reuse traversed prefixes without changing causal links.
- Streaming Observation now seals immutable text blocks at 16 KiB or approximately two seconds, batches event and summary writes, and compresses new blocks only when storage decreases. Existing rows remain unchanged. The inspector separates unsaved live previews from committed history, loads events incrementally, and pages older records. Ordinary detail reads no longer flush writers; Debug Bundle issuance flushes only the selected Interaction before fixing its complete snapshot. A process crash may lose pending observation text from the normal two-second window.
- Live observation canvases load summary-only interaction/root snapshots and fetch ordinary Run details only for the selected inspector. Context links use bounded batch queries and migration 42's equivalent SQLite/PostgreSQL partial indexes; filters, complete causal roots, and fixed time windows retain their existing semantics.
- Debug Wire and canonical checkpoints now enter Trace directly instead of adding an ordinary observation row per record. Four-direction ZIP capture, manifest updates, SSE replay, and fixed snapshot cutoffs remain available. Existing history is not rewritten or deleted.
- OpenCode Zen free-tier (`-free` suffix) models are filtered from discovery, capability queries, and record management, and intercepted in the execution path to prevent upstream 400 rejections.

### Fixed

- Request Records now aggregate chain-token filters once per root instead of repeating the same sum for each interaction, preserving thresholds, counts, and pagination. Canvas focus restores readable target cards independently of the whole-graph overview; programmatic positioning no longer pauses live follow or triggers edge pagination. Layouts reclaim space after branches disappear, and snapshot-scoped parent indexes reduce long-chain link and selection work without changing causal links or stored history.
- Target cooldown now recovers through one traffic-driven half-open probe rather than reopening to concurrent requests. Failed probes restart the configured cooldown without normal or provider-internal retries; complete success restores scheduling, while cancellation releases the probe and stale outcomes cannot overwrite a newer state. Removed the separate fixed 3-failure/30-second health filter. Model destination indicators now reflect runtime cooldown/probe states, refresh independently of unsaved edits, and show explicit read failures instead of stale green status.
- Usage analytics token totals (overview, hourly, model, and API Key aggregates) now accumulate reported values across successful Target attempts. Failed or still-running attempts never carry confirmed usage, and one attempt's unreported field no longer forces a whole group total to `null`; a total stays `null` only when no completed attempt reported that metric.
- Responses now preserves generic Thinking as full reasoning content in both streaming and non-streaming output and saved client history. Exact tool-result replays no longer split Interactions or incorrectly branch from older responses because of a summary/content mismatch. Native reasoning summaries remain distinct from full content; existing history and observation links are not rewritten.
- Usage analytics, API Key lists, and Model lists now complete their first load when dependency queries finish before the main query. Loading, refresh, and list error states read every participating query before combining results, preventing stale snapshots from leaving pages on skeletons or losing retry feedback.
- Codex V2 remote compaction now preserves native states completed by `response.output_item.done` when the terminal response omits them, instead of failing with `protocol_lossy_rejected`. Explicitly conflicting terminal states remain rejected, and legacy standalone compact errors are still forwarded without a local summary or implicit V2 conversion.
- Interaction status now accounts for unambiguous client-tool results on sibling Runs, without rewriting historical Run outcomes or masking genuinely pending branches. Database projections and Debug Bundles use the same evidence rule. Startup reconciles stale aggregates and marks unresolved waiting leaves **Interrupted** (`process_restarted`), preserving delivered history, retention, and later continuation.
- Waiting-client Interaction branches now become **Disconnected** when their WebSocket connection closes, without changing successful delivery or Generation Chain history. WebSocket finalization uses the stream task's final tool-handoff state, including closure races; later valid continuations can resume the same Interaction. Normal HTTP/SSE completion and older records without connection ownership are not treated as evidence of disconnection.
- Client-tool observations deduplicate replayed results only within a proven call boundary and retained, same-principal Run ancestry, comparing redacted contents and error state. New handoffs, sibling branches, changed results, and missing/null result boundaries remain distinct; migration 43 adds lookup indexes without copying stored bodies. Late inspector responses can no longer restore a closed or replaced selection.
- Generation history discovery now ignores application and internal tracking metadata while preserving complete message semantics, tool-result associations, protected reasoning, and unclassified protocol extensions. Startup rebuilds older derived prefix indexes without rewriting original history or parent links.
- Interaction Observation keeps exact pending-tool continuations in the same Interaction even when they include user reminders, without a time limit. Exact follow-ups received within two seconds of the parent response's complete delivery also stay together and can reactivate a completed Interaction; this diagnostic rule includes fast human follow-ups and does not alter model input or execution. Grouping reasons and immutable delivery timestamps survive observation delays and restarts.
- Streaming Responses reasoning items now seal before a new message, tool call, or ordinary item starts, keeping `output_item.done` ordering consistent with `output_index` and reopening a fresh reasoning item when consecutive reasoning blocks are interrupted.
- Tool-call completion events now emit as soon as an item finishes instead of waiting for the terminal response, without duplicate completions on response end.
- Markdown rendering escapes raw HTML (title, script, img, and similar), keeping model output visible without executing markup.
- Codex HTTP requests no longer send `service_tier: auto`, which upstream rejected with a 400 error.
- Devin pricing fields decode fixed32 values without float-precision noise.
- The desktop app disables Tauri's default drag handler so Windows HTML5 drag-and-drop works again.

## [0.2.2] - 2026-09-12

### Added

- Credential Protection supplements its Betterleaks snapshot with all 1,013 Kingfisher v1.109.0 offline rules, including bare Zhipu-format and additional `sk-` credentials. Capture selection, entropy, character requirements, safe-list filters, and checksums run locally; 152 hidden helper rules do not create mappings. The combined catalog and matching tester use the same detector as request protection, with no online validation or runtime rule downloads.
- WebUI and desktop applications now share the unified Cadence logo, wordmark, and favicons, with brand components and action-state motion replacing the legacy bitmap logos. The Windows taskbar and tray icons follow the system light/dark theme.

### Changed

- **Breaking:** The admin interface now defaults to accepting HTTP and HTTPS from any reachable origin while retaining the default loopback listener, same-origin/CSRF protection, and revocable sessions. Repeated `--admin-origin`/`STRAVIA_ADMIN_ORIGINS` restricts the entire admin surface, and repeated `--trusted-proxy`/`STRAVIA_TRUSTED_PROXIES` explicitly trusts actual proxy peers; HTTPS cookies follow each request's trusted external protocol. The former public-origin and Server admin-cors-origin configuration is removed, with Vite, Docker, and Nix deployment guidance migrated accordingly. Deployments accepting HTTP or unrestricted entry points must control their own transport-security and DNS-rebinding risks.

- Historical thinking now uses Target-local best-effort replay: incompatible ciphertext/signatures are omitted while visible text is retained, without rewriting original history. New reasoning records retain source bindings so returning to the original compatible Target can reuse ciphertext after intervening Targets or restarts.
- Windows MSVC builds use the Rust toolchain's bundled LLD linker while retaining static CRT linkage, full development debug information, and incremental compilation.
- Desktop builds emit only the Rust library consumed by the executable, avoiding unused C ABI static and dynamic library outputs during rebuilds.
- Added `task build:desktop:debug` for Rust-only desktop rebuilds without WebUI preparation or installer bundling; documented the incremental-build workflow for both applications.

### Fixed

- Inferred observation links now place continuations below their source using the same layout and connection geometry as confirmed links, distinguished only by a dashed stroke. Removed inference labels from canvas edges and card previews while retaining diagnostic details.
- Request-record diagnostic associations now preserve exact public thinking projections with History Markers across client model-instruction changes, instead of treating those public carriers as private-reasoning boundaries. Streaming and unary output snapshots share ingress history shaping with Generation Chain, matching the block structure clients replay. Execution ancestry, Principal isolation, private-reasoning exclusions, and ambiguity checks remain unchanged.
- Uncached Provider inventories now refresh global Catalog indexes before downloading, avoiding `CATALOG_SCOPE_REFRESH_FAILED` when local indexes lag behind the remote revision. Revision changes during downloads still fail without modifying saved Provider Models.
- Google browser search now exits early with an explicit automated-traffic challenge error when CAPTCHA or unusual-traffic pages are detected during preflight or result navigation, instead of waiting for the rendering timeout.
- Explicit encrypted-content or thinking-signature rejections before output permit one protected-reasoning-free replay; ordinary errors and native compaction do not. DeepSeek tool history retains existing reasoning and supplies an empty field when none was captured. Anthropic native replay accepts preserved `redacted_thinking` blocks.
- Ordinary chat messages carrying unknown provider fields now complete chat delivery and tool calls across protocol conversion, and same-protocol responses retain ordinary item additional fields. Unknown content types and native compaction states are still rejected as lossy conversions.
- Each OpenAI-compatible Thinking block now persists its own History Marker while preserving summary/content part boundaries and original whitespace, so preview formatting no longer contaminates replayed content. Streaming and unary projections use the same order, and stale preview fields are cleared once content enters canonical blocks.
- `StraviaRead` model tool schemas now list nullable optional arguments as required in strict mode, preventing Codex/Responses `invalid_function_parameters` failures while preserving optional fields for MCP callers.

## [0.2.1] - 2026-09-10

### Added

- Unified internal/S3 Artifact storage, stable Principal-scoped references, temporary signed downloads, and default-off upload assistance using upload-only fifteen-minute credentials. Settings and setup persist complete client/file addresses with matching English and Chinese loading, saving, and recovery states.

### Changed

- **Breaking:** `StraviaRead` replaces the former platform search/media tool names. URL-shaped inputs support Artifact downloads/questions, webpage Markdown, stored files, image understanding, and complete `query://` research; internal Agents use the same name for basic retrieval without recursive research. Existing capability preferences remain separate and enforced.
- Structured media is stored before model execution and materialized per actual Provider call. New history and diagnostics keep references rather than repeated base64; upload credentials are always protected, including expired replay. Migration 41 adds equivalent SQLite/PostgreSQL storage-location and download-grant metadata without rewriting historical media, preserving the already-applied migration 40 for interaction input previews.
- External signed downloads are explicitly opt-in for both client and Provider delivery. Native S3 signatures use the configured download endpoint; links need at least five minutes remaining before Provider calls. Active readers and unexpired grants protect physical cleanup without reviving expired files. Existing multipart limits remain 100 MiB per file and sixteen uploads / 400 MiB staging per Principal, with no saved-file aggregate quota.
- CI runs independent checks concurrently, caches compiled stable Rust test dependencies alongside the pinned toolchain when compiler fingerprints match, and lets the Windows desktop job populate a combined desktop/browser dependency cache. Prepared Admin and desktop tests no longer repeat their build prerequisites; all existing test coverage remains enabled.

### Fixed

- CI Rust cache keys no longer depend on unused toolchains preinstalled on runner images. Trusted main-branch manual runs can populate caches, and successful unit-test, browser-test, and Admin E2E compilation remains cacheable when subsequent tests fail. Admin cache warm-up builds debug and release profiles serially and includes development tools.
- The interaction observation E2E waits for the asynchronously persisted input preview before asserting its content, rather than treating Interaction creation as proof that the preview is ready.
- The Admin statistics E2E waits for persisted usage and duration before comparing totals, without converting unknown usage to zero or retrying inference requests.
- Existing databases with migration 40 for interaction input previews can upgrade to Artifact storage without a migration checksum conflict or any rewrite of applied migration history.
- Agent Definition registration no longer mistakes JSON object-key ordering differences between builds for an unversioned content change. Existing revision snapshots remain untouched, while real schema, instruction, tool, and policy changes still require a new revision.

## [0.2.0] - 2026-09-09

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
- **Breaking:** Local Search and Fetch now embed the pinned `moli-stealth` engine: `moli-stealth-net` handles HTTP and `moli-core` renders dynamic pages without an external browser. Browser-path settings and their admin endpoint are removed; old preference files are ignored. Proxy snapshots, cookie isolation, outbound URL/IP checks, and fetch limits remain enforced. Moli/V8 runs inside Stravia rather than Chrome's OS process sandbox; native builds require the documented toolchain, including Go 1.24 or newer.
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
