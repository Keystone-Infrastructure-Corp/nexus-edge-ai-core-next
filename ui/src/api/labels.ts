// COCO → domain-label catalog shared by the cameras + rules UIs.
//
// Mirror of `crates/nexus-inference/src/yolo.rs::
// map_coco_to_domain_label` — the engine emits exactly these
// strings when it runs the bundled YOLOv8-style detector against
// COCO. Listing the same strings here drives the in-form chooser
// so operators don't have to memorise the mapping (the most
// common foot-gun is typing `vehicle` instead of `vehicle.car`).
//
// Keep in sync with the engine table; the unit test
// `coco_table_known_ids` in yolo.rs anchors a couple of entries,
// but the full source of truth lives in that match arm.

export interface CocoLabel {
  /// COCO class id (kept for tooltip/debug — not required by the
  /// chooser UI but useful when an operator is cross-referencing
  /// upstream COCO tutorials).
  cocoId: number;
  /// The exact string the engine writes to `motion_events.label`
  /// and the rules engine compares with `object.label ==`.
  label: string;
  /// User-friendly grouping for the chooser UI. Lets us render
  /// the chips in clusters (people / vehicles / animals / carried)
  /// instead of a flat 12-item strip.
  group: "People" | "Vehicles" | "Animals" | "Carried";
}

/// Canonical list. Ordered so the most common labels appear first
/// within each group, then groups are presented People → Vehicles →
/// Animals → Carried (most → least common in security workflows).
export const COCO_DOMAIN_LABELS: ReadonlyArray<CocoLabel> = [
  { cocoId: 0, label: "person", group: "People" },
  { cocoId: 2, label: "vehicle.car", group: "Vehicles" },
  { cocoId: 7, label: "vehicle.truck", group: "Vehicles" },
  { cocoId: 5, label: "vehicle.bus", group: "Vehicles" },
  { cocoId: 3, label: "vehicle.motorcycle", group: "Vehicles" },
  { cocoId: 1, label: "vehicle.bicycle", group: "Vehicles" },
  { cocoId: 16, label: "animal.dog", group: "Animals" },
  { cocoId: 15, label: "animal.cat", group: "Animals" },
  { cocoId: 14, label: "animal.bird", group: "Animals" },
  { cocoId: 24, label: "carried.backpack", group: "Carried" },
  { cocoId: 26, label: "carried.handbag", group: "Carried" },
  { cocoId: 28, label: "carried.suitcase", group: "Carried" },
];

/// Flat list of just the string labels, in the same order. Handy
/// for `<datalist>` suggestions and `Set` membership checks.
export const COCO_DOMAIN_LABEL_STRINGS: ReadonlyArray<string> =
  COCO_DOMAIN_LABELS.map((l) => l.label);
