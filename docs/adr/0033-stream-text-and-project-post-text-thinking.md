---
status: accepted
---

# ADR-0033: Stream Text and project Post-Text Thinking through content

OpenAI-compatible Chat gives clients separate `reasoning_content` and `content` fields but no shared ordered block identity. Buffering visible Text until the Model Leg completed prevented field-based clients from moving later Thinking ahead of earlier Text. Wire captures showed the cost: clients received seconds of already-streamed Text only at completion, even when a transparent Platform Tool was exposed but never called.

**Decision.** Deliver visible Text immediately rather than buffering for possible tool use. The first non-empty Text in Client Projection starts Post-Text state for the entire Inference Run, including hidden Model Legs; it does not wait for client delivery. For Chat, put subsequent Platform and Thinking History Markers in `content` as HTML comments and stream only public Thinking as a delimited Markdown quote. Pre-Text Thinking can use the native carrier when it can round-trip losslessly; Chat's single reasoning field cannot preserve multiple independent blocks. Choose the carrier using information available at block start, not by buffering a leg to see whether a signature arrives. Other protocols use native carriers rather than this Chat-specific Markdown projection, and fail if they cannot represent the order.

Projected previews are presentation, not authoritative history. Persist the complete Thinking block and deliver and publish its Marker before releasing subsequent output; only output behind that specific unfinished block waits. Replay with the Marker restores the original block and drops the preview; if the user deletes the Marker, a surviving quote is ordinary Text. Marker failures terminate explicitly and prevent Generation Chain commit. Streaming and non-streaming use the same projection. This trades extra per-block persistence and visible comments in some clients for low latency and ordered, lossless replay without changing existing chain nodes.

The [architecture design](../design/architecture.md) owns protocol conversion and Client Projection.
