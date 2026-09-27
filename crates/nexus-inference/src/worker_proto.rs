//! Wire protocol between [`WorkerProcessBackend`](crate::backends::WorkerProcessBackend)
//! and the `nexus-inference-worker` child binary.
//!
//! Frames are encoded with `rmp-serde` (MessagePack) and prefixed by a
//! big-endian `u32` length on the stdio pipe. The worker speaks the same
//! protocol on stdin (requests) and stdout (responses); stderr is
//! reserved for diagnostics.
//!
//! Why MessagePack + a pipe?
//! - `Detection.attributes` is a map of `serde_json::Value`s with
//!   `#[serde(skip_serializing_if = "BTreeMap::is_empty")]`. Non-self-describing
//!   formats (bincode) can't round-trip that field, because skipping a
//!   field has no on-wire marker. MessagePack tags each field, so the
//!   deserializer correctly applies `#[serde(default)]` when the field
//!   is absent — and it also implements `deserialize_any`, which is what
//!   `serde_json::Value` needs.
//! - Pixel buffers (~1 MB at 640x480 RGB) stay byte-compact (MessagePack
//!   uses raw `bin` for `Vec<u8>`), unlike JSON's per-byte expansion.
//! - A pipe avoids fighting the kernel for a domain socket path in
//!   sandboxed test runs (looking at you, `/tmp` on macOS). Pipes are
//!   the universal substrate.
//!
//! M3 will swap this for shared-memory frame ring + a small control
//! socket carrying detection IDs, once we have a profile that shows the
//! pipe copy hurts. Until then the shape on this seam is intentionally
//! simple.

use std::io;

use chrono::{DateTime, Utc};
use nexus_types::{CameraId, Detection, Frame, FrameId, PixelFormat, TraceId};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum encoded message size we'll accept on the wire. Guards against a
/// crashing worker streaming garbage. 16 MB comfortably covers a 4K RGB
/// frame (~24 MB?) — bump if the workspace ever pushes 4K pixels through.
pub const MAX_MSG_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum WorkerRequest {
    /// Run detection. `req_id` is echoed back on the response so the
    /// parent can fan multiple in-flight requests over one pipe.
    Detect {
        req_id: u64,
        frame: WireFrame,
        prompts: Vec<String>,
    },
    /// Soft shutdown — worker writes nothing more after the next response.
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum WorkerResponse {
    DetectOk {
        req_id: u64,
        detections: Vec<Detection>,
    },
    DetectErr {
        req_id: u64,
        message: String,
    },
}

impl WorkerResponse {
    pub fn req_id(&self) -> u64 {
        match self {
            Self::DetectOk { req_id, .. } | Self::DetectErr { req_id, .. } => *req_id,
        }
    }
}

/// Serializable mirror of [`Frame`]. `Frame.data` is an `Arc<Vec<u8>>` so
/// it can't ride serde directly without an Arc shim — and on this seam
/// we explicitly want a copy on the wire.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireFrame {
    pub camera_id: CameraId,
    pub frame_id: FrameId,
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub trace_id: TraceId,
}

impl WireFrame {
    pub fn from_frame(f: &Frame) -> Self {
        Self {
            camera_id: f.camera_id,
            frame_id: f.frame_id,
            captured_at: f.captured_at,
            width: f.width,
            height: f.height,
            format: f.format,
            data: f.data.as_ref().clone(),
            trace_id: f.trace_id.clone(),
        }
    }

    pub fn into_frame(self) -> Frame {
        Frame {
            camera_id: self.camera_id,
            frame_id: self.frame_id,
            captured_at: self.captured_at,
            // Process-local, so it does not cross the pipe. The worker's
            // copy only feeds the detector, which reads no clock.
            captured_mono: std::time::Instant::now(),
            width: self.width,
            height: self.height,
            format: self.format,
            data: std::sync::Arc::new(self.data),
            trace_id: self.trace_id,
        }
    }
}

/// Encode + write one length-prefixed message to `w`. Flushes before
/// returning so the receiver sees a complete frame immediately.
pub async fn write_msg<W, M>(w: &mut W, msg: &M) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    M: Serialize,
{
    let bytes = rmp_serde::to_vec_named(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("msgpack encode: {e}")))?;
    if bytes.len() as u64 > MAX_MSG_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "message size {} bytes exceeds MAX_MSG_BYTES={}",
                bytes.len(),
                MAX_MSG_BYTES
            ),
        ));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read + decode one length-prefixed message from `r`.
pub async fn read_msg<R, M>(r: &mut R) -> io::Result<M>
where
    R: AsyncReadExt + Unpin,
    M: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MSG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("incoming message {len} bytes exceeds MAX_MSG_BYTES={MAX_MSG_BYTES}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    rmp_serde::from_slice::<M>(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("msgpack decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::{BBox, Detection};

    #[tokio::test]
    async fn roundtrip_response_through_duplex_pipe() {
        let resp = WorkerResponse::DetectOk {
            req_id: 42,
            detections: vec![Detection {
                label: "person".into(),
                confidence: 0.9,
                bbox: BBox {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 10.0,
                },
                attributes: Default::default(),
            }],
        };
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_msg(&mut a, &resp).await.unwrap();
        let got: WorkerResponse = read_msg(&mut b).await.unwrap();
        assert_eq!(got.req_id(), 42);
    }

    /// A detection's attribute keys are data: any string crosses the pipe,
    /// and the attributes come out as they went in.
    #[tokio::test]
    async fn roundtrip_detection_attributes_through_duplex_pipe() {
        let mut d = Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 0.0,
                y1: 0.0,
                x2: 10.0,
                y2: 10.0,
            },
            attributes: Default::default(),
        };
        d.attributes.insert("ppe.hardhat".into(), true.into());
        d.attributes.insert("".into(), 0.87.into());
        d.attributes
            .insert("ключ".into(), serde_json::json!({"b": [1, "x"], "a": null}));
        let want = serde_json::to_string(&d).unwrap();
        let resp = WorkerResponse::DetectOk {
            req_id: 7,
            detections: vec![d],
        };
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_msg(&mut a, &resp).await.unwrap();
        let WorkerResponse::DetectOk { detections, .. } = read_msg(&mut b).await.unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(serde_json::to_string(&detections[0]).unwrap(), want);
    }

    #[tokio::test]
    async fn roundtrip_request_with_frame() {
        let frame = WireFrame {
            camera_id: 1,
            frame_id: 7,
            captured_at: chrono::Utc::now(),
            width: 4,
            height: 4,
            format: nexus_types::PixelFormat::Rgb24,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            trace_id: "t".into(),
        };
        let req = WorkerRequest::Detect {
            req_id: 99,
            frame,
            prompts: vec!["a".into(), "b".into()],
        };
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_msg(&mut a, &req).await.unwrap();
        let got: WorkerRequest = read_msg(&mut b).await.unwrap();
        match got {
            WorkerRequest::Detect { req_id, .. } => assert_eq!(req_id, 99),
            _ => panic!("wrong variant"),
        }
    }
}
