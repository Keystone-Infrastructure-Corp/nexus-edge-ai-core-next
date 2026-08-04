// FleetManagedBadge (Phase 7.5.5) — shows a "Fleet-managed" pill on a
// settings page when the cloud control plane currently manages that
// category on this core.
//
// Phase 7.5.11: what the next fleet apply will do to local edits depends
// on the mode the cloud pushed with, so the tooltip says which it is.
// Under `replace` the cloud overwrites the whole category and local
// entries are deleted; under `merge` only the entries the fleet itself
// pushed are managed and operator-authored ones are left alone. A marker
// with no mode predates 7.5.11 and is read as `replace`.

import { useQuery } from "@tanstack/react-query";
import { Cloud } from "lucide-react";

import { listFleetManaged } from "@/api/admin";
import type { FleetCategory } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { formatAgo } from "@/lib/format";

export function FleetManagedBadge({ category }: { category: FleetCategory }) {
  const query = useQuery({
    queryKey: ["fleet-managed"],
    queryFn: listFleetManaged,
    staleTime: 30_000,
  });

  const marker = query.data?.managed.find((m) => m.category === category);
  if (!marker) return null;

  const scope = marker.scope_type
    ? `${marker.scope_type}${marker.scope_id ? ` ${marker.scope_id}` : ""}`
    : "fleet";
  const merge = marker.mode === "merge";
  const effect = merge
    ? "The fleet manages only the entries it pushed; your own entries are left alone."
    : "Local edits may be overwritten by the next fleet apply.";
  const title = `Managed by ${scope}${
    marker.applied_at ? ` · applied ${formatAgo(marker.applied_at)}` : ""
  } · ${merge ? "merge" : "replace"}. ${effect}`;

  return (
    <Badge variant="secondary" className="gap-1" title={title}>
      <Cloud className="h-3 w-3" />
      {merge ? "Fleet-managed (merge)" : "Fleet-managed"}
    </Badge>
  );
}
