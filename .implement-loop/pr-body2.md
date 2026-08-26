Fixes BUG-136 (record in the cloud repo).

The supervisor's analysis loop `await`s `detect` inline, and the only writer of `LatestFrameCache` sat after it. On `colmc-tx3604cedarspringsrd-1` (10 cameras, Apollo Lake, one detector worker) that put the cloud wall at **0.125 fps** — 16× below the 2 fps floor `gate.rs` documents — flapping stale/live on an 8s beat against the pump's 5s `STALL_AFTER`: 104 stale events on one camera in 27 minutes, while that camera recorded 18 clips and its `frame_id` climbed normally.

## The part that matters more than the rate

`mpsc` is FIFO and `try_send` drops the **newest** frame. A permanently-full 8-slot queue hands the loop a frame eight consumer-cycles old — **~64 s** at 8 s/inference. That is why the wall looked *behind* rather than merely slow, and it mis-stamps everything keyed on `frame.captured_at` (`motion_events`, `ClipFinal.ended_at`, `post_roll.tick`, alert thumbnails).

So the forward is `tokio::sync::watch` (depth-1, latest-wins), **not** a second FIFO. A second `try_send` stage would have doubled the lag to ~128 s and added 8 resident pixel buffers per camera (+125 MB to +318 MB RSS across ten cameras).

## Change

- **Tap task** between source and loop: publishes every decoded frame to the cache, forwards latest-wins to the loop.
- **Per-camera epoch** in `LatestFrameCache`. `abort()` is asynchronous, so without it a draining tap can repopulate a cache `stop_camera` just cleared — streaming a live feed of a *disabled* camera, strictly worse than the frozen frame the existing `clear()` test locks. Same shape as `is_current_session` (BUG-070).
- **`AbortOnDrop`** on the tap handle. Every `FrameSource` learns the camera is gone from `tx.closed()`, which only resolves when the receiver drops, and a bare `JoinHandle` *detaches*. Without the guard: source decodes at 15 fps forever, Ctrl-C hangs on the GStreamer bus thread, and a camera edit opens a second RTSP session against firmware documented as capping at one.
- **`observe_frame` moves into the tap** — the true "received from the source" point its own doc comment describes; inside the loop it was measuring inference rate.
- **`get_latest_frame_meta`** reports objects only when `objects_frame_id` matches the frame on screen.

## Honest outcome: ~1 fps, not 4

The pump's `scene_changed` is `objects_signature(entry.objects)` (`live_view.rs:423,428`), and objects stay on the inference clock, so emission falls through to the 1 Hz keepalive. Reaching `GRID_FPS` needs the pump's existing `frame_content_hash` wired in as the change signal, after which `DEFAULT_BUDGET_PER_SEC = 24` binds. **The frame's age is the real win — current instead of ~64 s old.**

Also fixes wall cells going dark during a detector wedge: `supervisor.rs:690` returns `Continue` on detector error *before* the post-inference cache write, so every cell blanked for the whole of BUG-135's 36-minute outage.

## Tests

- `tests/live_frame_freshness.rs` — runs the real supervisor against a detector whose future **never resolves**. Pre-fix the cache is written zero times and `get` stays `None`; post-fix the tap publishes anyway. A 0-vs-N discriminator, not a rate threshold, so it can't pass by luck on a slow CI box.
- `cache.rs` — retired-session write rejected after `clear`; superseded session can't overwrite the current one; never-inferred camera reports `objects_frame_id: None`; `put_frame` advances the frame without disturbing objects.

## Known follow-ups, not in this PR

- `gate.rs`'s module contract ("…and therefore the cloud live-view wall — runs at whatever rate this gate passes") no longer holds for the cache and needs rewriting.
- Overlays now visibly lag the video — honest, but operators will notice.
- The tap's dropped frames are a new, uncounted drop point (`observe_dropped` still only counts gate rejections).
- This is arguably ADR-worthy: splitting the wall's frame rate from the analysis rate is a durable decision.

No Rust toolchain on this workstation, so CI is the gate.
