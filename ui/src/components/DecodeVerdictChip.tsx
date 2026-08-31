import {
  AlertTriangle,
  Check,
  CircleHelp,
  Copy,
  Loader,
  Unplug,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  decodeVerdictPresentation,
  type VerdictPresentation,
} from "@/lib/decodeCapacity";
import { cn } from "@/lib/utils";

const ICONS = {
  check: Check,
  "alert-triangle": AlertTriangle,
  copy: Copy,
  unplug: Unplug,
  loader: Loader,
  help: CircleHelp,
} as const;

const TONE_CLASS = {
  ok: "text-emerald-600 dark:text-emerald-400",
  warn: "text-amber-600 dark:text-amber-400",
  bad: "text-destructive",
  muted: "text-muted-foreground",
} as const;

export function VerdictIcon({
  presentation,
  className,
}: {
  presentation: VerdictPresentation;
  className?: string;
}) {
  const Icon = ICONS[presentation.icon];
  return (
    <Icon
      aria-hidden
      className={cn("h-3.5 w-3.5 shrink-0", TONE_CLASS[presentation.tone], className)}
    />
  );
}

/**
 * Per-camera decode verdict (SPEC-069 Phase 1).
 *
 * Icon *plus* text, never colour alone. `read` is the query's own state, so a
 * pending or failed stats request renders "Not read" rather than an all-clear.
 */
export function DecodeVerdictChip({
  verdict,
  read,
  title,
}: {
  verdict: string | undefined;
  read: boolean;
  title?: string;
}) {
  const p = decodeVerdictPresentation(verdict, read);
  return (
    <Badge
      variant="outline"
      className="gap-1 font-normal"
      title={title}
      data-testid="decode-verdict-chip"
      data-verdict={p.verdict}
    >
      <VerdictIcon presentation={p} />
      <span>{p.label}</span>
    </Badge>
  );
}
