// CEL editor schema — single source of truth for autocomplete + docs.
//
// Mirrors what the engine actually evaluates against. The canonical
// definition lives in `crates/nexus-rules/tests/rule_corpus.rs` and
// `crates/nexus-tracker/src/annotator.rs`; this file ports the same
// taxonomy into a structured form the CodeMirror completion source
// can introspect.
//
// **Keep in sync**: when the annotator stamps a new attribute key or
// a new enum value, add it here AND ideally to the
// `/api/v1/rules/schema` server response so live-detected attributes
// keep working without a UI rebuild.

// ---------------------------------------------------------------------------
// Types.
// ---------------------------------------------------------------------------

/** Type tag used by the completion source to pick an icon and to drive
 *  the "what string values are valid here" suggestion path. */
export type CelType =
  | "string"
  | "number"
  | "bool"
  | "list"
  | "map"
  | "object"
  | "timestamp"
  | "duration";

export interface CelProperty {
  name: string;
  type: CelType;
  doc: string;
  /** For `map` types only: which key catalog to suggest after `[`. */
  keyCatalog?: AttributeKeyCatalog;
  /** For `string` types: enum values to suggest after `== '`. */
  enumValues?: readonly string[];
  /** Pointer into [`OBJECT_TYPES`] for nested objects. */
  objectType?: string;
}

export interface CelObjectType {
  properties: readonly CelProperty[];
}

export type AttributeKeyCatalog = "motion" | "object" | "group";

export interface AttributeKey {
  name: string;
  /** Value type the key resolves to. Used to drive `== 'foo'` enum
   *  suggestions when applicable. */
  type: CelType;
  doc: string;
  enumValues?: readonly string[];
}

// ---------------------------------------------------------------------------
// Top-level variables in CEL scope.
// ---------------------------------------------------------------------------

export interface CelVariable {
  name: string;
  type: CelType;
  doc: string;
  objectType?: string;
}

export const CEL_VARIABLES: readonly CelVariable[] = [
  {
    name: "object",
    type: "object",
    objectType: "Object",
    doc: "The detection currently being evaluated (label, confidence, bbox, attributes, age).",
  },
  {
    name: "track",
    type: "object",
    objectType: "Track",
    doc: "The track this detection belongs to. Stable across frames.",
  },
  {
    name: "camera",
    type: "object",
    objectType: "Camera",
    doc: "The camera that produced the frame.",
  },
  {
    name: "frame",
    type: "object",
    objectType: "Frame",
    doc: "The frame metadata (id, captured_at).",
  },
  {
    name: "now",
    type: "object",
    objectType: "Now",
    doc: "Wall-clock components from the rule-engine context: hour, day_of_week.",
  },
];

// ---------------------------------------------------------------------------
// Object type schemas.
// ---------------------------------------------------------------------------

export const OBJECT_TYPES: Record<string, CelObjectType> = {
  Object: {
    properties: [
      {
        name: "label",
        type: "string",
        doc: "Detector label, e.g. 'person', 'vehicle.car'.",
        // enumValues populated dynamically at runtime by the editor
        // when it has fetched the model prompt catalog.
      },
      {
        name: "confidence",
        type: "number",
        doc: "Detector confidence in [0.0, 1.0].",
      },
      {
        name: "box",
        type: "object",
        objectType: "Box",
        doc: "Bounding box in detector-frame coordinates (960x540).",
      },
      {
        name: "age_ms",
        type: "number",
        doc: "Milliseconds since the track was first observed.",
      },
      {
        name: "attributes",
        type: "map",
        keyCatalog: "motion",
        doc: "String-keyed map of annotator-stamped attributes (motion.*, group.*).",
      },
    ],
  },
  Box: {
    properties: [
      { name: "x1", type: "number", doc: "Top-left x in pixels." },
      { name: "y1", type: "number", doc: "Top-left y in pixels." },
      { name: "x2", type: "number", doc: "Bottom-right x in pixels." },
      { name: "y2", type: "number", doc: "Bottom-right y in pixels." },
      { name: "w", type: "number", doc: "Width in pixels." },
      { name: "h", type: "number", doc: "Height in pixels." },
    ],
  },
  Track: {
    properties: [
      { name: "id", type: "number", doc: "Stable per-camera track id." },
    ],
  },
  Camera: {
    properties: [
      { name: "id", type: "string", doc: "Camera id (e.g. '1', 'lot-north')." },
    ],
  },
  Frame: {
    properties: [
      { name: "id", type: "number", doc: "Monotonic per-camera frame id." },
      {
        name: "captured_at",
        type: "timestamp",
        doc: "Wall-clock capture time (UTC).",
      },
    ],
  },
  Now: {
    properties: [
      { name: "hour", type: "number", doc: "Hour of day, 0-23." },
      {
        name: "day_of_week",
        type: "number",
        doc: "Day of week, 0=Sunday, 6=Saturday.",
      },
    ],
  },
};

// ---------------------------------------------------------------------------
// Attribute keys (`object.attributes['<key>']`).
//
// The annotator stamps these per detection. Source of truth is
// `crates/nexus-tracker/src/annotator.rs`. Keep in lock-step.
// ---------------------------------------------------------------------------

export const ATTRIBUTE_KEYS: Record<AttributeKeyCatalog, readonly AttributeKey[]> = {
  motion: [
    {
      name: "motion.speed_class",
      type: "string",
      doc: "Coarse speed bucket from the annotator's px/s EMA.",
      enumValues: ["stationary", "walking", "running", "vehicle_speed"],
    },
    {
      name: "motion.direction",
      type: "string",
      doc: "8-way compass heading from the (dx, dy) EMA, or 'none' when stationary.",
      enumValues: ["n", "ne", "e", "se", "s", "sw", "w", "nw", "none"],
    },
    {
      name: "motion.parked_vehicle",
      type: "string",
      doc: "Only stamped on `vehicle.*` labels. 'yes' once movement EMA stays below threshold for N frames.",
      enumValues: ["yes", "no"],
    },
    {
      name: "motion.dwell_seconds",
      type: "number",
      doc: "Integer seconds since this track was first observed.",
    },
    {
      name: "motion.zone_state",
      type: "string",
      doc: "FSM over zone membership: outside -> entering -> inside -> exiting -> outside.",
      enumValues: ["outside", "entering", "inside", "exiting"],
    },
    {
      name: "motion.zone_ids",
      type: "list",
      doc: "List of inclusion/dwell zone ids the object is currently inside (post-transition).",
    },
    {
      name: "motion.near_static_vehicle_id",
      type: "string",
      doc: "Id of the nearest static anchor within proximity, or absent. Form 'label@cxXcy'.",
    },
    {
      name: "motion.near_static_vehicle_seconds",
      type: "number",
      doc: "Seconds this track has dwelt continuously near the same static anchor.",
    },
    {
      name: "motion.tool_in_proximity_label",
      type: "string",
      doc: "Lowercased label of the closest tool detection within reach, or '' when none.",
    },
    {
      name: "motion.tool_in_proximity_confidence",
      type: "number",
      doc: "Detector confidence of the nearby tool, when one is in proximity.",
    },
    {
      name: "motion.removed_anchor_ids",
      type: "list",
      doc: "Ids of static anchors present last frame but absent now (possible pickup).",
    },
    {
      name: "motion.carrying_anchor_label",
      type: "string",
      doc: "Label of the first removed anchor — pairs with removed_anchor_ids for pickup rules.",
    },
    {
      name: "group.size",
      type: "number",
      doc: "Count of OTHER same-label tracks within the proximity radius.",
    },
  ],
  // Reserved for future per-object / per-group attribute namespaces.
  object: [],
  group: [],
};

// ---------------------------------------------------------------------------
// CEL stdlib functions / methods. Top-level completions + dotted
// receiver methods (the latter when context allows).
// ---------------------------------------------------------------------------

export interface CelFunction {
  name: string;
  signature: string;
  doc: string;
  /** When true, also offered as a method on `string` receivers. */
  isStringMethod?: boolean;
}

export const CEL_FUNCTIONS: readonly CelFunction[] = [
  {
    name: "startsWith",
    signature: "string.startsWith(prefix: string) -> bool",
    doc: "True iff `string` starts with `prefix`.",
    isStringMethod: true,
  },
  {
    name: "endsWith",
    signature: "string.endsWith(suffix: string) -> bool",
    doc: "True iff `string` ends with `suffix`.",
    isStringMethod: true,
  },
  {
    name: "contains",
    signature: "string.contains(substr: string) -> bool",
    doc: "True iff `string` contains `substr`.",
    isStringMethod: true,
  },
  {
    name: "matches",
    signature: "string.matches(re: string) -> bool",
    doc: "True iff `string` fully matches the RE2 regular expression.",
    isStringMethod: true,
  },
  {
    name: "size",
    signature: "size(x) -> int",
    doc: "Length of a string, list, or map. Bytes for strings (UTF-8 code units).",
  },
  {
    name: "int",
    signature: "int(x) -> int",
    doc: "Coerce a number/string/timestamp to int. Truncates floats.",
  },
  {
    name: "double",
    signature: "double(x) -> double",
    doc: "Coerce an int/string to double.",
  },
  {
    name: "string",
    signature: "string(x) -> string",
    doc: "Coerce a value to its canonical string form.",
  },
  {
    name: "bool",
    signature: "bool(x) -> bool",
    doc: "Coerce a string ('true'/'false') to bool.",
  },
  {
    name: "timestamp",
    signature: "timestamp(s: string) -> timestamp",
    doc: "Parse an RFC3339 timestamp string.",
  },
  {
    name: "duration",
    signature: "duration(s: string) -> duration",
    doc: "Parse a duration like '1h', '500ms', '30s'.",
  },
  {
    name: "has",
    signature: "has(field) -> bool",
    doc: "True iff the receiver has the named field set (use for optional struct fields).",
  },
];

// ---------------------------------------------------------------------------
// Snippet templates — copy-paste-ready rule predicates.
// ---------------------------------------------------------------------------

export interface CelSnippet {
  label: string;
  detail: string;
  /** Template with `${placeholder}` markers for CodeMirror snippet expansion. */
  template: string;
}

export const CEL_SNIPPETS: readonly CelSnippet[] = [
  {
    label: "person · high confidence",
    detail: "Person seen with confidence ≥ 0.7",
    template: "object.label == 'person' && object.confidence >= ${0.7}",
  },
  {
    label: "running person",
    detail: "Person moving at running speed",
    template:
      "object.label == 'person' && object.attributes['motion.speed_class'] == 'running'",
  },
  {
    label: "vehicle in zone",
    detail: "Vehicle currently inside any inclusion zone",
    template:
      "object.label.startsWith('vehicle') && object.attributes['motion.zone_state'] == 'inside'",
  },
  {
    label: "vehicle entering zone",
    detail: "Vehicle on the frame it crosses INTO a zone",
    template:
      "object.label.startsWith('vehicle') && object.attributes['motion.zone_state'] == 'entering'",
  },
  {
    label: "parked vehicle",
    detail: "Vehicle whose movement EMA has dropped below threshold",
    template:
      "object.label.startsWith('vehicle') && object.attributes['motion.parked_vehicle'] == 'yes'",
  },
  {
    label: "loitering",
    detail: "Track has been observed for N+ seconds",
    template: "object.attributes['motion.dwell_seconds'] >= ${30}",
  },
  {
    label: "crowd forming",
    detail: "N+ other same-label tracks within proximity",
    template: "object.attributes['group.size'] >= ${3}",
  },
  {
    label: "after hours",
    detail: "Between 22:00 and 06:00 local",
    template: "now.hour >= ${22} || now.hour < ${6}",
  },
  {
    label: "weekend activity",
    detail: "Saturday or Sunday",
    template: "now.day_of_week == 0 || now.day_of_week == 6",
  },
  {
    label: "specific camera",
    detail: "Scope rule to one camera id",
    template: "camera.id == '${1}'",
  },
];
