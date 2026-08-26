//! COCO class-id → domain label mapping (closed-vocab label space).
//!
//! This is pure data shared by the ORT YOLO head, the Hailo head, and the
//! SPEC-040 label-space report. It lives in its own un-gated module so the
//! report can compute the closed vocab without pulling in the `ort`
//! feature (the detectors are `ort`-gated; the label *table* is not).

/// Map a COCO class id to its namespaced domain label, or `None` for a
/// class the domain does not surface.
pub fn map_coco_to_domain_label(class_id: i32) -> Option<&'static str> {
    Some(match class_id {
        0 => "person",
        1 => "vehicle.bicycle",
        2 => "vehicle.car",
        3 => "vehicle.motorcycle",
        5 => "vehicle.bus",
        7 => "vehicle.truck",
        14 => "animal.bird",
        15 => "animal.cat",
        16 => "animal.dog",
        24 => "carried.backpack",
        26 => "carried.handbag",
        28 => "carried.suitcase",
        _ => return None,
    })
}

/// The full closed-vocab (COCO→domain) label space the YOLO head emits.
///
/// SPEC-040 reports this so the cloud stops mirroring it by hand in
/// `map_coco_to_domain_label`'s cloud twin. Derived from
/// [`map_coco_to_domain_label`] itself — the one source of truth — by
/// walking the COCO class range, so it can never drift from the mapping.
pub fn closed_vocab_domain_labels() -> Vec<&'static str> {
    (0..=90).filter_map(map_coco_to_domain_label).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids_map() {
        assert_eq!(map_coco_to_domain_label(0), Some("person"));
        assert_eq!(map_coco_to_domain_label(2), Some("vehicle.car"));
        assert_eq!(map_coco_to_domain_label(99), None);
    }

    #[test]
    fn closed_vocab_is_the_twelve_domain_labels() {
        let v = closed_vocab_domain_labels();
        assert_eq!(v.len(), 12);
        assert!(v.contains(&"person"));
        assert!(v.contains(&"carried.suitcase"));
    }
}
