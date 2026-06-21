// FleetManagedBadge (Phase 7.5.5) — shows a "Fleet-managed" pill on a
// settings page when the cloud control plane currently manages that
// category on this core. The edge applies fleet settings with REPLACE
// semantics (cloud config overwrites local edits), so this badge warns
// the operator that local changes to the category may be overwritten by
// the next fleet apply.

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
  const title = `Managed by ${scope}${
    marker.applied_at ? ` · applied ${formatAgo(marker.applied_at)}` : ""
  }. Local edits may be overwritten by the next fleet apply.`;

  return (
    <Badge variant="secondary" className="gap-1" title={title}>
      <Cloud className="h-3 w-3" />
      Fleet-managed
    </Badge>
  );
}
