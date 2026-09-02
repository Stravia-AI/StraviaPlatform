---
status: accepted
---

# ADR-0033: Stream Text and project Post-Text Thinking through content

## Context

ADR-0030 preserved `Thinking → Text → Platform Tool → Thinking → Text` order for OpenAI-compatible clients by buffering visible Text after Platform Tool exposure until the current Model Leg completed. That prevented field-based clients from moving later `reasoning_content` ahead of earlier `content`, but it also made ordinary answers appear non-streaming whenever transparent Platform Tools were available.

Observed wire captures confirmed the cost: upstream Text streamed continuously for several seconds while the client received the same bytes only after upstream completion. The common case—an exposed Platform Tool that is never called—paid the full latency despite having no ordering ambiguity to resolve.

OpenAI-compatible chat chunks provide separate `reasoning_content` and `content` fields but no ordered block identity shared across both fields. Once non-empty Text has been delivered, later Thinking cannot be carried in `reasoning_content` without risking reordering by clients that aggregate fields independently.

## Decision

Supersede ADR-0030 and adopt immediate Text delivery as the primary invariant.

- Ordinary visible Text is never buffered solely because a Platform Tool is exposed or later used.
- The first successfully delivered non-empty Text starts a run-wide Post-Text state. It persists across hidden Model Legs within the same Inference Run.
- Before that transition, OpenAI-compatible Thinking and History Markers remain in `reasoning_content`.
- After that transition, Platform Markers and Thinking Markers use `content` as raw HTML comments.
- Public Post-Text Thinking is streamed in `content` as a Markdown blockquote inside the existing Projection Delimiter `preview` mode.
- Every canonical Post-Text Thinking block has one authoritative Thinking History Marker. Without native block identity, one maximal continuous Thinking delta run is one block.
- The Marker reference is reserved in memory when the block starts. At block close, Stravia atomically stores the complete authoritative Thinking under that reference, delivers and publishes the Marker, then permits later Text.
- The quoted Preview is presentation only. Replay discards it when its Marker is present and restores authoritative Thinking. If the Marker is deleted, the remaining quote is ordinary Text.
- Only already-public Thinking bytes may be previewed. Protected payloads remain in the History Marker Store; without a public summary, the client receives only the Marker.
- Projection-generated whitespace, blockquote prefixes and escaping remain within the Preview Delimiter span and are removed during replay.
- A bounded lexical lookbehind may retain enough bytes to handle line boundaries, CRLF and private Marker/Delimiter prefixes split across deltas. This is encoding state, not Model Leg Text buffering.
- Marker persistence, delivery or publish failure after Preview delivery terminates the stream explicitly and prevents Generation Chain commit. Preview bytes are never downgraded to canonical Text.
- Streaming and non-streaming OpenAI-compatible responses must materialize the same Client Projection.
- Open Responses and other protocols retain native Thinking/Text carriers. If an adapter cannot represent Post-Text Thinking in order, it fails explicitly rather than converting to Markdown or restoring the old Text buffer.
- Existing Generation Chain nodes remain readable and immutable. New output uses only the new projection. No database migration is introduced.

The removal is narrow. Protocol framing, UTF-8 completion, partial private syntax handling, ToolCall name classification, protected Thinking accumulation, Marker ordering barriers and terminal Hook buffering remain.

## Consequences

### Positive

- Text remains genuinely streaming when transparent Platform Tools are enabled, including when no tool is called.
- The latency cost is limited to the specific Post-Text Thinking block that requires authoritative Marker finalization, not the entire Model Leg.
- OpenAI-compatible field aggregation preserves the order of earlier Text, Platform Markers, quoted later Thinking and subsequent Text.
- Multi-turn replay remains lossless because History Markers, not Markdown Preview text, are authoritative.
- The design reuses the existing History Marker Store and Projection Delimiter without a schema migration or a new durable pending state.

### Negative

- Plain-text clients may display raw HTML Marker comments carried in `content`.
- Post-Text public Thinking is presented as quoted content rather than through a dedicated reasoning field.
- The streaming projector needs stateful Markdown framing and bounded cross-delta syntax detection.
- A storage or publish failure can occur after a Preview has already been delivered, requiring an explicit terminal error and leaving the client with a visibly partial failed response.
- Protocols that cannot natively represent the observed order now fail explicitly instead of receiving a buffered approximation.

### Compatibility

- Old reasoning-carried History Markers and Text Projection Delimiters remain replayable for their existing retention period.
- New responses do not dual-write the old projection.
- Platform Tool execution, public client-tool ownership, Hooks, response identity, usage and terminal semantics are unchanged except where explicit projection failure prevents a successful terminal commit.
