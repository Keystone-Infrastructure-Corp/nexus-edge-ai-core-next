---
applyTo: "models/**,crates/nexus-reid/**,crates/nexus-store/**,crates/nexus-storage/**"
---

# Privacy and model rules

- No face-recognition models at the edge in v1. `xtask check-models` fails on
  AdaFace, ArcFace, InsightFace, Buffalo, FaceNet, SphereFace, CosFace and MagFace.
- Re-ID produces appearance embeddings only (DINOv2-S default, OSNet-x1.0 opt-in).
  It must never produce face-recognition embeddings.
- Every weights file needs a manifest artifact `path`. A file no manifest names
  fails the check, whatever its licence.
- Do not persist personal identifiers in the local SQLite store outside the
  operator labels table.
- Add no new non-trivial Rust dependency without a licence and binary-size
  note in the PR description.
