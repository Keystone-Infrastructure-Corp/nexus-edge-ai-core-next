// BUG-225 — one reading of `GET /api/v1/health`, shared by the TopBar pill
// and the dashboard's Engine tile so the two cannot disagree.
//
// The verdict comes from the query's state, never from a defaulted value: a
// request that has not answered is `unread`, which is neither ok nor
// degraded. Only an answer the query actually read carries the response.

import type { HealthIssue, HealthResponse } from "@/api/types";

export type EngineHealth =
  | { verdict: "unread" }
  | { verdict: "unreachable" }
  | { verdict: "ok" | "degraded"; read: HealthResponse };

export function engineHealth(q: {
  isError: boolean;
  isSuccess: boolean;
  data: HealthResponse | undefined;
}): EngineHealth {
  if (q.isError) return { verdict: "unreachable" };
  if (!q.isSuccess || q.data === undefined) return { verdict: "unread" };
  return { verdict: q.data.status === "ok" ? "ok" : "degraded", read: q.data };
}

/**
 * One issue's name: its code, verbatim, plus the detector kind when it has
 * one. A detector's detail need not say which kind failed.
 */
export function issueName(issue: HealthIssue): string {
  return issue.kind ? `${issue.code} (${issue.kind})` : issue.code;
}

/** The distinct issue codes, verbatim, so a code this UI has no copy for still shows. */
export function issueCodes(issues: HealthIssue[]): string {
  return [...new Set(issues.map((i) => i.code))].join(", ");
}
