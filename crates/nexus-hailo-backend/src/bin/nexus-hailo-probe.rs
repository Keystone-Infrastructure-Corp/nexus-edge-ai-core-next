//! `nexus-hailo-probe` — standalone diagnostic for the Hailo backend.
//!
//! Usage:
//!     nexus-hailo-probe                            # just list devices
//!     nexus-hailo-probe --hef path/to/yolo26n.hef   # also open + dummy infer
//!
//! Exits non-zero on any failure. Designed for `journalctl`-friendly
//! line output rather than pretty TUI.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use nexus_hailo_backend::{InferSession, OutputLayout};
use tracing_subscriber::{fmt, EnvFilter};

fn main() -> ExitCode {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .init();

    if !nexus_hailo_backend::is_supported() {
        eprintln!(
            "nexus-hailo-probe: this build has no HailoRT linkage \
             (need linux + --features linked)"
        );
        return ExitCode::from(2);
    }

    // --- enumerate devices ---
    match InferSession::devices() {
        Ok(devs) if devs.is_empty() => {
            eprintln!("nexus-hailo-probe: no Hailo devices found");
            return ExitCode::from(3);
        }
        Ok(devs) => {
            for (i, d) in devs.iter().enumerate() {
                println!(
                    "device[{i}]: board={} serial={} fw={}.{}.{} part={}",
                    d.board_name,
                    d.serial,
                    d.fw_version.0,
                    d.fw_version.1,
                    d.fw_version.2,
                    d.device_id,
                );
            }
        }
        Err(e) => {
            eprintln!("nexus-hailo-probe: device enumeration failed: {e}");
            return ExitCode::from(1);
        }
    }

    // --- if --hef given, open + run one dummy frame ---
    let hef_path = parse_hef_arg();
    if let Some(path) = hef_path {
        match probe_hef(&path) {
            Ok(()) => {
                println!("nexus-hailo-probe: OK");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("nexus-hailo-probe: HEF probe failed: {e}");
                ExitCode::from(1)
            }
        }
    } else {
        ExitCode::SUCCESS
    }
}

fn parse_hef_arg() -> Option<PathBuf> {
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--hef" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

fn probe_hef(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("opening HEF: {}", path.display());
    let mut session = InferSession::open(path, None, None)?;
    let (h, w, c) = session.input_shape();
    println!(
        "input_shape: {h}x{w}x{c}  input_frame_size: {}  output_frame_size: {}",
        session.input_frame_size(),
        session.output_frame_size(),
    );
    for info in session.output_infos() {
        println!(
            "  output: name={:<24} shape={}x{}x{}  frame_size={}",
            info.name, info.h, info.w, info.c, info.frame_size,
        );
    }
    match session.output_layout() {
        OutputLayout::NmsByClass {
            num_classes,
            max_bboxes_per_class,
        } => println!(
            "output_layout: NMS_BY_CLASS  classes={num_classes}  max/class={max_bboxes_per_class}"
        ),
        OutputLayout::NmsByScore { max_bboxes_total } => {
            println!("output_layout: NMS_BY_SCORE  max_total={max_bboxes_total}")
        }
        OutputLayout::RawYolo26 { num_classes, scales } => {
            println!(
                "output_layout: RAW_YOLO26  classes={num_classes}  scales={}",
                scales.len()
            );
            for s in scales {
                println!(
                    "    scale stride={} grid={}x{} box_idx={} score_idx={}",
                    s.stride, s.h, s.w, s.box_idx, s.score_idx
                );
            }
        }
        OutputLayout::Other => {
            println!("output_layout: Other (unsupported by YOLO postproc)")
        }
    }

    // Push a zeroed dummy frame and decode the result. yolo26n on a
    // black image typically returns 0 detections, which validates the
    // wire path end-to-end without needing a real camera frame.
    let input = vec![0u8; session.input_frame_size()];
    let layout = session.output_layout().clone();
    let buffers = session.infer_blocking(&input)?;
    let detections = nexus_hailo_backend::decode_detections(buffers, &layout, 200);
    println!("dummy-frame detections: {}", detections.len());
    for (i, d) in detections.iter().enumerate().take(5) {
        println!(
            "  [{i}] class={} score={:.3} box=({:.3},{:.3})-({:.3},{:.3})",
            d.class_id, d.score, d.x_min, d.y_min, d.x_max, d.y_max,
        );
    }
    Ok(())
}
