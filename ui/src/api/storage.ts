// Storage + Delivery API wrappers (Phase 5).

import { api } from "@/api/client";
import type {
  AdminSinksResp,
  AdminSinkView,
  ColdReplicaState,
  DeliverySettings,
  OAuthProvider,
  OAuthStartReq,
  OAuthStartResp,
  OAuthStatusResp,
  PutAdminDeliveryReq,
  PutBackendReq,
  PutColdReq,
  PutRuleDeliveryReq,
  RuleDeliveryResp,
  SinkConfig,
  SinksHealthResp,
  StorageBackendOut,
  StorageResponse,
  TestSinkOut,
  UsbPreferredOut,
} from "@/api/types";

// --- Storage ---------------------------------------------------------------

export function getStorage() {
  return api.get<StorageResponse>("/storage");
}

export function putBackend(handle: string, req: PutBackendReq) {
  return api.put<StorageBackendOut>(
    `/admin/storage/backends/${encodeURIComponent(handle)}`,
    req,
  );
}

export function deleteBackend(handle: string) {
  return api.delete<void>(
    `/admin/storage/backends/${encodeURIComponent(handle)}`,
  );
}

export function putColdReplica(req: PutColdReq) {
  return api.put<ColdReplicaState>("/admin/storage/cold", req);
}

export function putUsbPreferred(label: string | null) {
  return api.put<UsbPreferredOut>("/admin/runtime/usb_preferred", { label });
}

// --- OAuth ----------------------------------------------------------------

export function startOAuth(provider: OAuthProvider, req: OAuthStartReq) {
  return api.post<OAuthStartResp>(
    `/admin/oauth/${provider}/start`,
    req,
  );
}

export function getOAuthStatus(state: string) {
  return api.get<OAuthStatusResp>("/admin/oauth/status", {
    query: { state },
  });
}

// --- Delivery -------------------------------------------------------------

export function getDeliverySettings() {
  return api.get<DeliverySettings>("/admin/delivery");
}

export function putDeliverySettings(req: PutAdminDeliveryReq) {
  return api.put<DeliverySettings>("/admin/delivery", req);
}

export function getRuleDelivery(ruleId: string) {
  return api.get<RuleDeliveryResp>(
    `/rules/${encodeURIComponent(ruleId)}/delivery`,
  );
}

export function putRuleDelivery(ruleId: string, req: PutRuleDeliveryReq) {
  return api.put<void>(
    `/rules/${encodeURIComponent(ruleId)}/delivery`,
    req,
  );
}

export function getSinksHealth() {
  return api.get<SinksHealthResp>("/admin/sinks/health");
}

// --- Sink configuration ---------------------------------------------------

/**
 * Sentinel the engine substitutes for every secret field in an admin
 * GET response, and which a PUT echoes back to mean "leave the stored
 * secret unchanged". MUST match
 * `nexus_config::SinkConfig::REDACTED_SECRET`.
 */
export const REDACTED_SECRET = "__nexus_secret_redacted__";

export function getSinks() {
  return api.get<AdminSinksResp>("/admin/sinks");
}

/**
 * Create or replace a runtime sink. The path id must agree with the
 * body — the engine rejects a mismatch with 400 so a UI bug can't
 * upsert under the wrong key. A successful PUT makes the engine
 * rebuild its live `SinkRegistry` without a restart.
 */
export function putSink(kind: string, name: string, config: SinkConfig) {
  return api.put<AdminSinkView>(
    `/admin/sinks/config/${encodeURIComponent(kind)}/${encodeURIComponent(name)}`,
    config,
  );
}

/** Deletes a runtime sink. 404s for a sink pinned in nexus.toml. */
export function deleteSink(kind: string, name: string) {
  return api.delete<void>(
    `/admin/sinks/config/${encodeURIComponent(kind)}/${encodeURIComponent(name)}`,
  );
}

/**
 * Fires one synthetic alert at the LIVE sink instance (real secrets
 * and all), bypassing the outbox. Resolves with HTTP 200 for both a
 * successful and a failed delivery — the outcome rides in the body.
 */
export function testSink(kind: string, name: string) {
  return api.post<TestSinkOut>(
    `/admin/sinks/config/${encodeURIComponent(kind)}/${encodeURIComponent(name)}/test`,
    {},
  );
}
