//! Cheap motion gate.
//!
//! The gate is the cheapest layer that can drop work. It downsamples the
//! Y plane (or RGB→Y) by 8×, computes per-pixel absolute delta against the
//! previous frame, and returns true when the changed-pixel count exceeds
//! a threshold. Roughly 0.3 ms per 1080p frame on a recent CPU.

use std::sync::Mutex;

use nexus_types::{Frame, PixelFormat};

pub struct MotionGate {
    prev_y: Mutex<Option<Vec<u8>>>,
    delta_threshold: u8,
    pixel_pct_threshold: f32,
    /// Force pass every Nth frame so motionless cams still send an inference
    /// occasionally (for tracker TTL refresh, scene-change fallback).
    keyframe_every: u32,
    state: Mutex<u32>,
}

impl MotionGate {
    pub fn new() -> Self {
        Self {
            prev_y: Mutex::new(None),
            delta_threshold: 16,
            pixel_pct_threshold: 0.005,
            keyframe_every: 30,
            state: Mutex::new(0),
        }
    }

    pub fn allow(&self, frame: &Frame) -> bool {
        // Periodic keyframe pass-through.
        let mut s = self.state.lock().unwrap();
        *s = s.wrapping_add(1);
        if *s % self.keyframe_every == 0 {
            // Still update prev_y so the next frame has a baseline.
            let y = downsample_y(frame);
            *self.prev_y.lock().unwrap() = Some(y);
            return true;
        }
        let y = downsample_y(frame);
        let mut prev = self.prev_y.lock().unwrap();
        let allow = if let Some(prev_y) = prev.as_ref() {
            if prev_y.len() != y.len() {
                true
            } else {
                let mut changed = 0usize;
                for (a, b) in prev_y.iter().zip(y.iter()) {
                    if a.abs_diff(*b) > self.delta_threshold {
                        changed += 1;
                    }
                }
                let pct = changed as f32 / y.len() as f32;
                pct >= self.pixel_pct_threshold
            }
        } else {
            true
        };
        *prev = Some(y);
        allow
    }
}

impl Default for MotionGate {
    fn default() -> Self {
        Self::new()
    }
}

/// 8× downsample to a Y-only buffer. Cheap nearest-neighbour pick.
fn downsample_y(frame: &Frame) -> Vec<u8> {
    let scale = 8u32;
    let dw = (frame.width / scale).max(1) as usize;
    let dh = (frame.height / scale).max(1) as usize;
    let stride = frame.stride();
    let mut out = Vec::with_capacity(dw * dh);
    let data = frame.data.as_ref();

    match frame.format {
        PixelFormat::Nv12 | PixelFormat::I420 => {
            for j in 0..dh {
                let sy = (j * scale as usize).min(frame.height as usize - 1);
                for i in 0..dw {
                    let sx = (i * scale as usize).min(frame.width as usize - 1);
                    out.push(data[sy * stride + sx]);
                }
            }
        }
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => {
            for j in 0..dh {
                let sy = (j * scale as usize).min(frame.height as usize - 1);
                for i in 0..dw {
                    let sx = (i * scale as usize).min(frame.width as usize - 1);
                    let off = sy * stride + sx * 3;
                    if off + 2 < data.len() {
                        let r = data[off] as u32;
                        let g = data[off + 1] as u32;
                        let b = data[off + 2] as u32;
                        // Cheap luma approximation.
                        out.push(((r * 30 + g * 59 + b * 11) / 100) as u8);
                    } else {
                        out.push(0);
                    }
                }
            }
        }
    }
    out
}
