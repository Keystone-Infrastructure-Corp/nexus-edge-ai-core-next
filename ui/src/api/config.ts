// API wrappers for Phase 4 configuration pages (cameras, rules, visual
// prompts, discovery, prompt catalog).
//
// Kept thin — TanStack Query owns caching & retries.

import { api } from "@/api/client";
import type {
  CameraConfig,
  CameraVisualPromptAttachment,
  CelValidateResponse,
  DiscoverySessionCreated,
  DiscoverySessionView,
  ModelPromptsCatalog,
  PreviewRuleRequest,
  PreviewRuleResponse,
  ProbeOnvifRequest,
  ProbeOnvifResult,
  ProbeRtspRequest,
  ProbeRtspResult,
  RuleConfig,
  ScanRequest,
  VisualPrompt,
  VisualPromptSummary,
} from "@/api/types";

// ---------------------------------------------------------------------------
// Cameras CRUD.
// ---------------------------------------------------------------------------

/// Create a new camera. Server assigns the `id`; the body field
/// is ignored. Returns the populated config with the new id.
export function createCamera(camera: CameraConfig): Promise<CameraConfig> {
  return api.post<CameraConfig>("/cameras", camera);
}

/// Update an existing camera. `camera.id` MUST be a non-zero
/// positive i64 that already exists in the engine. For creates
/// use `createCamera`. SPEC-088 — with `expectedZonesRevision` the
/// engine saves only while the stored zones still have that revision;
/// otherwise it answers 409 `zones_revision_conflict` and writes nothing.
export function updateCamera(
  camera: CameraConfig,
  expectedZonesRevision?: string,
): Promise<CameraConfig> {
  return api.put<CameraConfig>(
    `/cameras/${encodeURIComponent(String(camera.id))}`,
    camera,
    { query: { expected_zones_revision: expectedZonesRevision } },
  );
}

/// Smart upsert that routes to POST for new cameras (id <= 0)
/// and PUT for existing ones. Most call sites should use this
/// directly; `createCamera` / `updateCamera` are available for
/// the rare flows that know upfront which they want.
export function upsertCamera(
  camera: CameraConfig,
  expectedZonesRevision?: string,
): Promise<CameraConfig> {
  if (!camera.id || camera.id <= 0) {
    return createCamera(camera);
  }
  return updateCamera(camera, expectedZonesRevision);
}

export function deleteCamera(id: number): Promise<void> {
  return api.delete<void>(`/cameras/${encodeURIComponent(String(id))}`);
}

// ---------------------------------------------------------------------------
// Rules CRUD.
// ---------------------------------------------------------------------------

export function listRules(): Promise<RuleConfig[]> {
  return api.get<RuleConfig[]>("/rules");
}

/// Create a new rule. Server assigns the `id`; any `id` set on the
/// body is ignored. Returns the populated rule with the new id so
/// the caller can update local state without a second roundtrip.
/// Mirrors `createCamera` / `POST /cameras`.
export function createRule(rule: RuleConfig): Promise<RuleConfig> {
  return api.post<RuleConfig>("/rules", rule);
}

export function upsertRule(rule: RuleConfig): Promise<RuleConfig> {
  return api.put<RuleConfig>(
    `/rules/${encodeURIComponent(rule.id)}`,
    rule,
  );
}

/// Smart save that routes to `POST /rules` for new rules (empty id)
/// and `PUT /rules/{id}` for existing ones. Mirrors `upsertCamera`.
/// Most call sites in the UI should use this.
export function saveRule(rule: RuleConfig): Promise<RuleConfig> {
  if (!rule.id || !rule.id.trim()) {
    return createRule(rule);
  }
  return upsertRule(rule);
}

export function deleteRule(id: string): Promise<void> {
  return api.delete<void>(`/rules/${encodeURIComponent(id)}`);
}

export function validateRuleCel(when: string): Promise<CelValidateResponse> {
  return api.post<CelValidateResponse>("/rules/validate", { when });
}

/** Live CEL editor schema — labels emittable by the loaded detector
 *  + attribute keys the annotator stamps. Used to enrich the static
 *  completion source so newly-added attributes appear without a UI
 *  rebuild. Cached aggressively; falls back to the static schema
 *  when the engine is unreachable. */
export interface RulesSchema {
  labels: string[];
  attribute_keys: string[];
}

export function getRulesSchema(): Promise<RulesSchema> {
  return api.get<RulesSchema>("/rules/schema");
}

export function previewRule(
  req: PreviewRuleRequest,
): Promise<PreviewRuleResponse> {
  return api.post<PreviewRuleResponse>("/rules/preview", req);
}

// ---------------------------------------------------------------------------
// Visual prompts.
// ---------------------------------------------------------------------------

export function listVisualPrompts(): Promise<VisualPromptSummary[]> {
  return api.get<VisualPromptSummary[]>("/admin/visual-prompts");
}

export function getVisualPrompt(id: string): Promise<VisualPrompt> {
  return api.get<VisualPrompt>(
    `/admin/visual-prompts/${encodeURIComponent(id)}`,
  );
}

/**
 * Create a visual prompt via multipart upload.
 * Fields: name (text), description? (text), image (File).
 */
export function uploadVisualPrompt(opts: {
  name: string;
  description?: string;
  image: File;
}): Promise<VisualPrompt> {
  const fd = new FormData();
  fd.set("name", opts.name);
  if (opts.description) fd.set("description", opts.description);
  fd.set("image", opts.image);
  return api.post<VisualPrompt>("/admin/visual-prompts", fd);
}

export function deleteVisualPrompt(id: string): Promise<void> {
  return api.delete<void>(
    `/admin/visual-prompts/${encodeURIComponent(id)}`,
  );
}

export function listCameraVisualPrompts(
  cameraId: string,
): Promise<CameraVisualPromptAttachment[]> {
  return api.get<CameraVisualPromptAttachment[]>(
    `/admin/cameras/${encodeURIComponent(cameraId)}/visual-prompts`,
  );
}

export function attachVisualPrompt(
  cameraId: string,
  visualPromptId: string,
): Promise<void> {
  return api.post<void>(
    `/admin/cameras/${encodeURIComponent(cameraId)}/visual-prompts/${encodeURIComponent(visualPromptId)}`,
  );
}

export function detachVisualPrompt(
  cameraId: string,
  visualPromptId: string,
): Promise<void> {
  return api.delete<void>(
    `/admin/cameras/${encodeURIComponent(cameraId)}/visual-prompts/${encodeURIComponent(visualPromptId)}`,
  );
}

// ---------------------------------------------------------------------------
// Discovery.
// ---------------------------------------------------------------------------

export function startOnvifDiscovery(): Promise<DiscoverySessionCreated> {
  return api.post<DiscoverySessionCreated>(
    "/admin/discovery/onvif",
    {},
  );
}

export function startCidrScan(
  req: ScanRequest,
): Promise<DiscoverySessionCreated> {
  return api.post<DiscoverySessionCreated>(
    "/admin/discovery/scan",
    req,
  );
}

export function getDiscoverySession(
  sessionId: string,
): Promise<DiscoverySessionView> {
  return api.get<DiscoverySessionView>(
    `/admin/discovery/sessions/${encodeURIComponent(sessionId)}`,
  );
}

export function probeRtsp(
  sessionId: string,
  req: ProbeRtspRequest,
): Promise<ProbeRtspResult> {
  return api.post<ProbeRtspResult>(
    `/admin/discovery/sessions/${encodeURIComponent(sessionId)}/probe-rtsp`,
    req,
  );
}

/// Ask the engine to enumerate streams via the camera's own
/// ONVIF Media `GetProfiles` + `GetStreamUri` calls. The engine
/// tries Media2 first then falls back to Media1; always returns
/// HTTP 200 — callers check `.ok`. On failure the UI falls
/// back to `probeRtsp` (the brute-force path sweep).
export function probeOnvifStreams(
  sessionId: string,
  req: ProbeOnvifRequest,
): Promise<ProbeOnvifResult> {
  return api.post<ProbeOnvifResult>(
    `/admin/discovery/sessions/${encodeURIComponent(sessionId)}/onvif-streams`,
    req,
  );
}

// ---------------------------------------------------------------------------
// Prompt catalog.
// ---------------------------------------------------------------------------

export function getModelPromptsCatalog(): Promise<ModelPromptsCatalog> {
  return api.get<ModelPromptsCatalog>("/models/prompts");
}
