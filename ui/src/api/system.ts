// API wrappers for endpoints used by Dashboard / System / Backends.
//
// All exported functions return parsed JSON; throw `ApiError` on
// non-2xx. They're intentionally thin so TanStack Query owns
// caching + retries.

import { api } from "@/api/client";
import type {
  AlertEvent,
  BackendsResponse,
  CameraConfig,
  FrameMetadata,
  HealthResponse,
  MotionEventRow,
  MotionHistogramBucket,
  OutboxRow,
  StaticAnchorsResponse,
  SystemMetrics,
} from "@/api/types";

// ---------------------------------------------------------------------------
// Health.
// ---------------------------------------------------------------------------

export function getHealth(): Promise<HealthResponse> {
  return api.get<HealthResponse>("/health");
}

// ---------------------------------------------------------------------------
// System metrics (admin-or-viewer; the engine accepts any role).
// ---------------------------------------------------------------------------

export function getSystemMetrics(): Promise<SystemMetrics> {
  return api.get<SystemMetrics>("/system/metrics");
}

// ---------------------------------------------------------------------------
// Cameras.
// ---------------------------------------------------------------------------

export function listCameras(): Promise<CameraConfig[]> {
  return api.get<CameraConfig[]>("/cameras");
}

export function getLatestFrameMeta(cameraId: string): Promise<FrameMetadata> {
  return api.get<FrameMetadata>(
    `/cameras/${encodeURIComponent(cameraId)}/frames/latest.json`,
  );
}

/** Absolute URL for a JPEG snapshot — used directly as an `<img src>`. */
export function latestFrameJpegUrl(cameraId: string): string {
  return `/api/v1/cameras/${encodeURIComponent(cameraId)}/frames/latest`;
}

// Per-camera live frame stats — fps EMA, frames_emitted/dropped,
// source dims. Backed by `crates/nexus-pipeline/src/stats.rs`
// updated on every frame in the supervisor loop. `null` fields
// when the supervisor hasn't produced a frame yet.
export interface CameraFrameStats {
  camera_id: number;
  last_frame_at: string | null;
  last_frame_age_ms: number | null;
  fps_ema: number;
  frames_emitted: number;
  frames_dropped: number;
  source_width: number;
  source_height: number;
  // SPEC-069 Phase 1 decode counters and verdict. Optional so an older
  // engine (or a core that has not produced a frame yet) parses cleanly.
  decoder_input_drops?: number;
  decoder_output_frames?: number;
  sampled_frames?: number;
  duplicate_frames?: number;
  duplicate_per_mille?: number;
  decoder_output_fps?: number;
  sampled_fps?: number;
  decoder_width?: number;
  decoder_height?: number;
  /** `healthy` | `losing_frames` | `padded` | `substream_unavailable` | `measuring`. */
  decode_verdict?: string;
  /** The operator sentence the engine computed; thresholds live only there. */
  decode_summary?: string;
  decode_ceiling_fps?: number | null;
  analysis_stream?: AnalysisStreamStatus | null;
}

/**
 * Which stream the analysis tap is reading, per camera (SPEC-069 Phase 2).
 *
 * Note the shipped string values, which differ from the spec's prose:
 * `mode` is `mainstream`/`substream`, and `state` is
 * `active`/`probing`/`unavailable` — a degraded substream reverts to the
 * main stream rather than lingering, so there is no `degraded` state.
 */
export interface AnalysisStreamStatus {
  mode: string;
  state: string;
  reason?: string;
  width: number;
  height: number;
  fps: number;
}

export function getCameraStats(cameraId: string | number): Promise<CameraFrameStats> {
  return api.get<CameraFrameStats>(
    `/cameras/${encodeURIComponent(String(cameraId))}/stats`,
  );
}

/** Every camera's stats in one call, so an N-camera list costs one request. */
export function listCameraStats(): Promise<CameraFrameStats[]> {
  return api.get<CameraFrameStats[]>("/cameras/stats");
}

// Per-camera static-object map — anchor centroids for every
// vehicle the engine has promoted to "static" (e.g. parked car).
// Centroids are in detector-frame pixel coordinates (same
// system as `TrackedObject.bbox`), so the viewer overlay can
// reuse the existing `toX` / `toY` transform.
export function getStaticAnchors(
  cameraId: string | number,
): Promise<StaticAnchorsResponse> {
  return api.get<StaticAnchorsResponse>(
    `/cameras/${encodeURIComponent(String(cameraId))}/static-anchors`,
  );
}

// Operator-initiated wipe of the persisted + in-memory
// static-object map for one camera. Used by the viewer's
// "Clear anchors" button when stale anchors remain after a
// vehicle drove off occluded. Returns 204 — no body. The
// supervisor applies the wipe on its next frame; on a quiet
// camera, an immediate re-`GET` may still see the pre-clear
// state for ~one frame.
export function clearStaticAnchors(cameraId: string | number): Promise<void> {
  return api.delete<void>(
    `/cameras/${encodeURIComponent(String(cameraId))}/static-anchors`,
  );
}

// Engine-wide defaults for tracker.static_object. Used by the
// camera settings form to render "Engine default: Ns" next to
// the per-camera `behavior.anchor_ttl_secs` override input.
// Snapshot at engine boot — restart-required.
export interface StaticObjectDefaults {
  anchor_ttl_secs: number;
}

export function getStaticObjectDefaults(): Promise<StaticObjectDefaults> {
  return api.get<StaticObjectDefaults>("/system/static-object-defaults");
}

// ---------------------------------------------------------------------------
// Events.
// ---------------------------------------------------------------------------

/**
 * M-Event-Audit: an event-list row is the recorded `AlertEvent` plus its
 * `alerted` classification — `true` when the match fell within the delivery
 * schedule (an alert), `false` for an audit-only off-schedule match.
 */
export type EventListItem = AlertEvent & { alerted: boolean };

export function listEvents(
  limit = 50,
  alerted?: boolean,
): Promise<EventListItem[]> {
  const query: Record<string, string | number> = { limit };
  if (alerted !== undefined) query.alerted = alerted ? "true" : "false";
  return api.get<EventListItem[]>("/events", { query });
}

export function getEventDelivery(eventId: string): Promise<OutboxRow[]> {
  return api.get<OutboxRow[]>(
    `/events/${encodeURIComponent(eventId)}/delivery`,
  );
}

export function getEventClipId(
  eventId: string,
): Promise<{ clip_id: number }> {
  return api.get<{ clip_id: number }>(
    `/events/${encodeURIComponent(eventId)}/clip`,
  );
}

/** Absolute URL for an MP4 clip — used as `<video src>`. Supports Range. */
export function clipUrl(clipId: number | string): string {
  return `/api/v1/clips/${encodeURIComponent(String(clipId))}`;
}

// ---------------------------------------------------------------------------
// Motion / timeline.
// ---------------------------------------------------------------------------

export interface MotionRangeParams {
  from?: string;
  to?: string;
  limit?: number;
}

export function listCameraMotion(
  cameraId: string,
  params: MotionRangeParams = {},
): Promise<MotionEventRow[]> {
  return api.get<MotionEventRow[]>(
    `/cameras/${encodeURIComponent(cameraId)}/motion`,
    { query: { ...params } },
  );
}

export interface MotionHistogramParams {
  from?: string;
  to?: string;
  bucket_seconds?: number;
}

export function getCameraMotionHistogram(
  cameraId: string,
  params: MotionHistogramParams = {},
): Promise<MotionHistogramBucket[]> {
  return api.get<MotionHistogramBucket[]>(
    `/cameras/${encodeURIComponent(cameraId)}/motion/histogram`,
    { query: { ...params } },
  );
}

// ---------------------------------------------------------------------------
// Backends.
// ---------------------------------------------------------------------------

export function getBackends(): Promise<BackendsResponse> {
  return api.get<BackendsResponse>("/backends");
}
