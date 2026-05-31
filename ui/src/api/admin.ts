// Admin API wrappers (Phase 6 + M-Admin Phase 0): users, audit, runtime
// (bind / auth / OIDC discovery / diagnostics tarball).

import { api, getAccessToken } from "@/api/client";
import type {
  CreateUserRequest,
  CreateUserResponse,
  ListAuditQuery,
  ListAuditResponse,
  ListUsersResponse,
  ResetPasswordResponse,
  UpdateUserRequest,
  UserView,
} from "@/api/types";

// --- Users ----------------------------------------------------------------

export function listUsers(includeDeleted = false) {
  return api.get<ListUsersResponse>("/admin/users", {
    query: includeDeleted ? { include_deleted: true } : undefined,
  });
}

export function createUser(req: CreateUserRequest) {
  return api.post<CreateUserResponse>("/admin/users", req);
}

export function updateUser(id: string, req: UpdateUserRequest) {
  return api.put<UserView>(`/admin/users/${encodeURIComponent(id)}`, req);
}

export function resetUserPassword(id: string) {
  return api.post<ResetPasswordResponse>(
    `/admin/users/${encodeURIComponent(id)}/reset-password`,
    {},
  );
}

export function unlockUser(id: string) {
  return api.post<void>(
    `/admin/users/${encodeURIComponent(id)}/unlock`,
    {},
  );
}

export function deleteUser(id: string) {
  return api.delete<void>(`/admin/users/${encodeURIComponent(id)}`);
}

// --- Audit ----------------------------------------------------------------

export function listAudit(q: ListAuditQuery = {}) {
  return api.get<ListAuditResponse>("/admin/audit", {
    query: q as Record<string, string | number | boolean | undefined>,
  });
}

// --- Server bind (M-Admin Phase 0, restart-based) -------------------------

/**
 * Pending state for the optional UI alias listener. `null` on the
 * parent struct means "no persisted override" (engine will use TOML
 * at next boot). When present, `action` disambiguates "explicit off"
 * from "explicit set" since both differ from the on-disk default.
 */
export type UiBindPending =
  | { action: "set"; addr: string }
  | { action: "clear" };

/**
 * Operator-supplied update for the UI alias listener. Omit the
 * `ui_bind` field on the PUT entirely to leave the persisted row
 * alone; otherwise the discriminator chooses between persisting a
 * new addr (`set`), persisting explicit "off" (`clear`), or
 * dropping the override row so the engine falls back to TOML
 * (`reset`).
 */
export type UiBindUpdate =
  | { action: "set"; addr: string }
  | { action: "clear" }
  | { action: "reset" };

export interface ServerBindOut {
  current: string;
  pending: string | null;
  /** What the engine bound the UI alias to at boot. `null` = no second listener. */
  ui_current: string | null;
  /** Pending change for the UI alias listener; `null` = no change queued. */
  ui_pending: UiBindPending | null;
}

export interface PutServerBindOut {
  current: string;
  pending: string;
  ui_current: string | null;
  ui_pending: UiBindPending | null;
  restart_required: boolean;
}

export function getServerBind() {
  return api.get<ServerBindOut>("/admin/server/bind");
}

export function putServerBind(addr: string, ui_bind?: UiBindUpdate) {
  // serialise as `undefined` (omitted) when the operator hasn't
  // touched the UI bind editor — keeps the wire shape stable.
  const body: { addr: string; ui_bind?: UiBindUpdate } = { addr };
  if (ui_bind !== undefined) body.ui_bind = ui_bind;
  return api.put<PutServerBindOut>("/admin/server/bind", body);
}

// --- Auth config (M-Admin Phase 0, restart-based) -------------------------
//
// We use `unknown` for the wire AuthConfig shape on PUT because the engine
// is the source of truth for the schema — the UI builds it from the
// individual form fields and we don't want a long-lived parallel TS type.

export type AuthMode = "local" | "oidc" | "hybrid";

export interface SafeOidcConfig {
  issuer: string;
  audience: string;
  jwks_uri?: string;
  client_id?: string;
  display_name?: string;
  scopes: string[];
  role_claims: string[];
  deny_unmapped: boolean;
  redirect_uri?: string;
}

export interface SafeAuthConfig {
  mode: AuthMode;
  oidc?: SafeOidcConfig;
  admin_secret_path?: string;
}

export interface AuthConfigOut {
  current: SafeAuthConfig;
  pending: SafeAuthConfig | null;
}

export interface PutAuthConfigOut {
  restart_required: boolean;
  mode: AuthMode;
  oidc_issuer?: string;
}

export function getAuthConfig() {
  return api.get<AuthConfigOut>("/admin/auth/config");
}

export function putAuthConfig(body: unknown) {
  return api.put<PutAuthConfigOut>("/admin/auth/config", body);
}

// --- OIDC discovery dry-run -----------------------------------------------

export interface TestDiscoveryOut {
  ok: boolean;
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
  jwks_uri: string;
  userinfo_endpoint?: string;
  supports_pkce_s256: boolean;
}

export function testOidcDiscovery(req: {
  issuer: string;
  audience?: string;
  jwks_uri?: string;
}) {
  return api.post<TestDiscoveryOut>(
    "/admin/auth/oidc/test-discovery",
    req,
  );
}

// --- Diagnostics tarball (streaming) --------------------------------------
//
// The endpoint streams `Content-Type: application/gzip` with a
// `Content-Disposition: attachment; filename="..."` header. We use the
// `asBlob` codepath so the entire response body buffers as a Blob ready
// to hand to URL.createObjectURL, then derive the filename from the
// header so the saved file matches what the engine generated.

export interface DiagnosticsBundle {
  blob: Blob;
  filename: string;
}

export async function downloadDiagnosticsBundle(
  query?: { audit_limit?: number; motion_limit?: number },
): Promise<DiagnosticsBundle> {
  // We can't use `api.get<Blob>(..., { asBlob: true })` here because we
  // also need the Content-Disposition header to recover the engine's
  // intended filename. Drop down to fetch + manually attach the bearer
  // token via the shared accessor in the client module.
  const params = new URLSearchParams();
  if (query?.audit_limit !== undefined) {
    params.set("audit_limit", String(query.audit_limit));
  }
  if (query?.motion_limit !== undefined) {
    params.set("motion_limit", String(query.motion_limit));
  }
  const qs = params.toString();
  const url = `/api/v1/admin/diagnostics/export${qs ? `?${qs}` : ""}`;

  const token = getAccessToken();
  const res = await fetch(url, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!res.ok) {
    throw new Error(
      `diagnostics export failed: HTTP ${res.status} ${res.statusText}`,
    );
  }
  const blob = await res.blob();
  const filename = parseContentDispositionFilename(
    res.headers.get("content-disposition"),
  ) ?? `nexus-diagnostics-${nowStamp()}.tar.gz`;
  return { blob, filename };
}

function parseContentDispositionFilename(header: string | null): string | null {
  if (!header) return null;
  // Match the simple `filename="foo.tar.gz"` form the engine emits.
  const m = /filename\s*=\s*"?([^";]+)"?/i.exec(header);
  if (!m || m[1] === undefined) return null;
  return m[1].trim();
}

function nowStamp(): string {
  const d = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${d.getUTCFullYear()}${pad(d.getUTCMonth() + 1)}${pad(d.getUTCDate())}-`
    + `${pad(d.getUTCHours())}${pad(d.getUTCMinutes())}${pad(d.getUTCSeconds())}`
  );
}


// --- Storage watermarks (M-Admin Phase 0, restart-based) ------------------

export interface WatermarkOut {
  low_pct: number;
  panic_pct: number;
  pending_low_pct: number | null;
  pending_panic_pct: number | null;
}

export interface PutWatermarkOut {
  current_low_pct: number;
  current_panic_pct: number;
  pending_low_pct: number;
  pending_panic_pct: number;
  restart_required: boolean;
}

export function getWatermarks() {
  return api.get<WatermarkOut>("/admin/server/watermarks");
}

export function putWatermarks(low_pct: number, panic_pct: number) {
  return api.put<PutWatermarkOut>("/admin/server/watermarks", {
    low_pct,
    panic_pct,
  });
}


// --- Phase C: operator-set engine display name ----------------------------

export interface IdentityOut {
  /** `null` when no name set — the cloud falls back to the
   * fingerprint suffix for display. */
  display_name: string | null;
}

export function getServerIdentity() {
  return api.get<IdentityOut>("/admin/server/identity");
}

export function putServerIdentity(display_name: string | null) {
  return api.put<IdentityOut>("/admin/server/identity", { display_name });
}


// --- Phase 5.6 · R7 — re-identification local diagnostic -----------------
//
// `GET /v1/admin/reid/status` pairs the boot-time `[reid]` config
// with live per-camera emit counters drawn from the worker's stats
// registry. Used by `/admin/reid` to answer "is the re-ID worker
// actually firing for this camera right now?" — drives the field
// dogfood workflow on edge boxes.

export interface ReidCameraStatusRow {
  camera_id: number;
  emit_count: number;
  last_emit_at: string | null;
  last_embedding_hex8: string;
}

export interface ReidStatusResponse {
  enabled: boolean;
  model_id: string;
  dim: number;
  emit_interval_s: number;
  min_track_age_frames: number;
  cameras: ReidCameraStatusRow[];
}

export function getReidStatus() {
  return api.get<ReidStatusResponse>("/admin/reid/status");
}


// --- Default inference model (M-Admin Phase 0 follow-up, restart-based) ---

export interface InferenceModelView {
  kind: string;
  preset: string;
  input_width: number;
  input_height: number;
  score_threshold: number;
  pack_path?: string;
}

export interface InferenceModelOut {
  current: InferenceModelView;
  pending: InferenceModelView | null;
  available_kinds: string[];
}

export interface PutInferenceModelOut {
  current: InferenceModelView;
  pending: InferenceModelView;
  restart_required: boolean;
}

export interface InferenceModelPatch {
  kind?: string;
  preset?: string;
  input_width?: number;
  input_height?: number;
  score_threshold?: number;
  /** Empty string clears any persisted pack_path. */
  pack_path?: string;
}

export function getInferenceModel() {
  return api.get<InferenceModelOut>("/admin/server/inference");
}

export function putInferenceModel(patch: InferenceModelPatch) {
  return api.put<PutInferenceModelOut>("/admin/server/inference", patch);
}


// --- Cloud enrollment (M-Cloud Phase 1) -----------------------------------
//
// Engine contract:
//   GET  /api/v1/admin/cloud/enrollment  → CloudEnrollmentStatus
//   POST /api/v1/admin/cloud/enroll      → CloudEnrollmentStatus
//
// Both endpoints are AdminContext-gated. POST runs the same
// `cloud_enroll::perform_enrollment` flow as the
// `nexus-engine enroll` CLI subcommand and persists the result into the
// local `cloud_enrollment` SQLite row (overwriting any prior
// enrollment). Restart-required: the WSS tunnel is spawned exactly once
// at boot from the persisted row, so a successful enroll surfaces in
// the UI as a "Restart engine to activate the tunnel" affordance.
//
// The status response is intentionally redacted — never includes the
// mTLS private key or the entitlement JWT, so it's safe to ship to the
// browser.

export interface CloudEnrollmentStatus {
  enrolled: boolean;
  core_id?: string;
  gateway_url?: string;
  /** RFC3339 timestamp of the last successful enrollment round-trip. */
  enrolled_at?: string;
}

export interface PostCloudEnrollReq {
  /** Short single-use enrollment code from the cloud console's "Add Core" flow. */
  code: string;
  /** Base URL of the cloud console (must be https:// in production). */
  cloud_host: string;
  /** Human-friendly label baked into the CSR's CommonName. Defaults to hostname. */
  label?: string;
  /**
   * When true, the local motion-clip backlog from the past
   * `history_days` days will be replayed into the cloud on the next
   * engine boot. Defaults to false so most operators don't end up with
   * pre-cloud noise in their fresh console.
   */
  keep_history?: boolean;
  /** 1..=365 days. Defaults to 30. Ignored when `keep_history` is false. */
  history_days?: number;
}

export function getCloudEnrollment() {
  return api.get<CloudEnrollmentStatus>("/admin/cloud/enrollment");
}

export function postCloudEnroll(req: PostCloudEnrollReq) {
  return api.post<CloudEnrollmentStatus>("/admin/cloud/enroll", req);
}


// --- Engine self-restart (M-Admin Phase 0 follow-up) ----------------------

export interface RestartOut {
  restart_scheduled: boolean;
  delay_ms: number;
  current_bind: string;
}

export function restartEngine(delay_ms?: number) {
  return api.post<RestartOut>(
    "/admin/server/restart",
    delay_ms === undefined ? {} : { delay_ms },
  );
}


// --- OS-level network (M-Admin Phase 0 NIC manager) ----------------------
//
// Three concerns the existing /server/bind handler does NOT cover, all
// served by this group:
//
//   - Enumerate the OS's NICs (so the bind dropdown can pick from real
//     interfaces instead of asking the operator to type a host:port).
//   - Edit the persisted netplan plan (ethernets + VLANs) so the engine
//     can run on a secure VLAN while the admin UI alias runs on an open
//     one.
//   - Apply the plan with a 120s "netplan try"-style auto-rollback so a
//     bad plan can't lock the operator out.
//
// All endpoints require admin role. The mutation endpoints (`put`,
// `apply`, `confirm`, `rollback`) are audited server-side.

export type InterfaceKind =
  | "physical"
  | "vlan"
  | "bridge"
  | "bond"
  | "wireless"
  | "loopback"
  | "other";

export interface InterfaceAddr {
  addr: string;
  prefix_len: number;
  family: "ipv4" | "ipv6";
}

export interface NetworkInterface {
  name: string;
  mac?: string;
  addrs: InterfaceAddr[];
  is_loopback: boolean;
  operstate?: string;
  carrier?: boolean;
  mtu?: number;
  kind: InterfaceKind;
  parent?: string;
  vlan_id?: number;
}

export interface NameserversWire {
  addresses?: string[];
  search?: string[];
}

export interface EthernetConfigWire {
  dhcp4?: boolean;
  addresses?: string[];
  gateway?: string;
  nameservers?: NameserversWire;
  mtu?: number;
  macaddress?: string;
}

export interface VlanConfigWire {
  id: number;
  link: string;
  dhcp4?: boolean;
  addresses?: string[];
  gateway?: string;
  nameservers?: NameserversWire;
  mtu?: number;
}

/**
 * Operator-facing netplan plan. Matches the engine's curated
 * subset — anything fancier than per-NIC static/dhcp + VLAN
 * sub-interfaces lives in an operator-managed
 * `/etc/netplan/99-operator.yaml` and is out of UI scope.
 */
export interface NetplanPlan {
  ethernets?: Record<string, EthernetConfigWire>;
  vlans?: Record<string, VlanConfigWire>;
}

export interface ApplySession {
  apply_token: string;
  started_at: string;
  rollback_at: string;
}

export interface InterfacesOut {
  interfaces: NetworkInterface[];
}

export interface PlanOut {
  plan: NetplanPlan;
  apply_pending?: ApplySession;
}

export interface ApplyOut {
  session: ApplySession;
}

export interface ApplyStatusOut {
  session?: ApplySession;
}

export function listNetworkInterfaces() {
  return api.get<InterfacesOut>("/admin/network/interfaces");
}

export function getNetworkPlan() {
  return api.get<PlanOut>("/admin/network/plan");
}

export function putNetworkPlan(plan: NetplanPlan) {
  return api.put<PlanOut>("/admin/network/plan", plan);
}

export function applyNetworkPlan() {
  return api.post<ApplyOut>("/admin/network/plan/apply", {});
}

export function confirmNetworkApply(apply_token: string) {
  return api.post<void>("/admin/network/plan/confirm", { apply_token });
}

export function rollbackNetworkApply() {
  return api.post<void>("/admin/network/plan/rollback", {});
}

export function getNetworkApplyStatus() {
  return api.get<ApplyStatusOut>("/admin/network/apply/status");
}
