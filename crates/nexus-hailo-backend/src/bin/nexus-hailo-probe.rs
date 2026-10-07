//! `nexus-hailo-probe` — standalone diagnostic for the Hailo backend.
//!
//! Usage:
//!     nexus-hailo-probe                            # just list devices
//!     nexus-hailo-probe --hef path/to/yolo26n.hef   # also open + dummy infer
//!     nexus-hailo-probe --hef m.hef --images DIR --csv OUT.csv
//!                                                   # run raw RGB frames, one row per detection
//!     ... --stats                                   # also print raw output stats for the first frame
//!     nexus-hailo-probe --version                   # print the release version and exit 0
//!
//! Exits non-zero on any failure, including an unrecognized argument.
//! Designed for `journalctl`-friendly line output rather than pretty TUI.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use nexus_hailo_backend::{InferSession, OutputLayout};
use tracing_subscriber::{fmt, EnvFilter};

const USAGE: &str =
    "usage: nexus-hailo-probe [--version] [--hef PATH [--images DIR --csv OUT.csv] [--stats]]";

fn main() -> ExitCode {
    let args = match parse_args(env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("nexus-hailo-probe: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    if args.version {
        println!("nexus-hailo-probe {}", env!("NEXUS_BUILD_VERSION"));
        return ExitCode::SUCCESS;
    }

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
                // HailoRT does not populate `board_name` for every part;
                // printing an empty `board=` field is noise, not signal.
                if d.board_name.is_empty() {
                    println!(
                        "device[{i}]: serial={} fw={}.{}.{} part={}",
                        d.serial, d.fw_version.0, d.fw_version.1, d.fw_version.2, d.device_id,
                    );
                } else {
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
        }
        Err(e) => {
            eprintln!("nexus-hailo-probe: device enumeration failed: {e}");
            return ExitCode::from(1);
        }
    }

    // --- if --hef given, open + run one dummy frame ---
    if let Some(path) = args.hef {
        match probe_hef(
            &path,
            args.images.as_deref(),
            args.csv.as_deref(),
            args.stats,
        ) {
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

/// Parsed command-line arguments. Kept as a tiny pure function (no
/// `std::env` access) so the parsing logic -- in particular, rejecting
/// an unrecognized argument instead of silently ignoring it (edge#353)
/// -- is unit-testable without a subprocess.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    version: bool,
    hef: Option<PathBuf>,
    images: Option<PathBuf>,
    csv: Option<PathBuf>,
    stats: bool,
}

fn parse_args<I>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut out = Args::default();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" => out.version = true,
            "--stats" => out.stats = true,
            "--hef" => {
                out.hef =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "--hef requires a path argument".to_string()
                    })?));
            }
            "--images" => {
                out.images =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "--images requires a directory argument".to_string()
                    })?));
            }
            "--csv" => {
                out.csv =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        "--csv requires a path argument".to_string()
                    })?));
            }
            other => return Err(format!("unrecognized argument '{other}'")),
        }
    }
    Ok(out)
}

/// `--stats`: per-output min / max / mean / fraction-positive of the dequantised
/// values for the first frame. Shows what the chip actually emits before the
/// floor and NMS -- the question a zero-detection result cannot answer on its own.
fn print_output_stats(name: &str, out_names: &[String], raw: &[Vec<u8>]) {
    println!("stats for {name}:");
    for (out, buf) in out_names.iter().zip(raw) {
        let v: Vec<f32> = buf
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if v.is_empty() {
            println!("  {out:<28} n=0");
            continue;
        }
        let (mut lo, mut hi, mut sum, mut pos) = (f32::MAX, f32::MIN, 0f64, 0usize);
        for &x in &v {
            lo = lo.min(x);
            hi = hi.max(x);
            sum += x as f64;
            pos += (x > 0.0) as usize;
        }
        println!(
            "  {:<28} n={:<7} min={lo:.4} max={hi:.4} mean={:.4} positive={:.3}%",
            out,
            v.len(),
            sum / v.len() as f64,
            100.0 * pos as f64 / v.len() as f64,
        );
    }
}

/// Run every raw frame in `dir` and print one CSV row per detection.
///
/// Frames are raw interleaved RGB of exactly `input_frame_size()` bytes --
/// no image decoding here, because this crate has four dependencies and
/// decoding JPEG is not worth a fifth. Convert with any tool that can
/// write raw bytes at the model's input resolution.
///
/// The point of running real frames through THIS binary rather than a
/// separate harness is that it exercises the production decode path
/// (`decode_detections`), so a comparison between two HEFs measures the
/// model change and not a reimplementation.
fn run_images(
    session: &mut InferSession,
    layout: &OutputLayout,
    dir: &std::path::Path,
    csv: &std::path::Path,
    stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let want = session.input_frame_size();
    let out_names: Vec<String> = session
        .output_infos()
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let mut unlisted = 0usize;
    let mut files: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(dir)? {
        match e {
            Ok(e) if e.path().is_file() => files.push(e.path()),
            Ok(_) => {}
            Err(_) => unlisted += 1,
        }
    }
    files.sort();
    // Rows go to their own file: stdout also carries the device/HEF preamble
    // and the final "OK" line, and mixing them made the output unparseable.
    let mut out = std::io::BufWriter::new(std::fs::File::create(csv)?);
    writeln!(out, "image,class_id,score,x_min,y_min,x_max,y_max")?;
    let (mut run, mut wrong_size, mut failed) = (0usize, 0usize, 0usize);
    for f in &files {
        let buf = match std::fs::read(f) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("nexus-hailo-probe: skipping {}: {e}", f.display());
                failed += 1;
                continue;
            }
        };
        if buf.len() != want {
            wrong_size += 1;
            continue;
        }
        let name = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let raw = match session.infer_blocking(&buf) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("nexus-hailo-probe: inference failed on {name}: {e}");
                failed += 1;
                continue;
            }
        };
        if run == 0 && stats {
            print_output_stats(&name, &out_names, raw);
        }
        for d in nexus_hailo_backend::decode_detections(raw, layout, 200) {
            writeln!(
                out,
                "{name},{},{:.6},{:.6},{:.6},{:.6},{:.6}",
                d.class_id, d.score, d.x_min, d.y_min, d.x_max, d.y_max,
            )?;
        }
        run += 1;
    }
    out.flush()?;
    // Every frame is accounted for, so a short CSV cannot pass for a full one.
    println!(
        "images: {run} run, {wrong_size} wrong size (expected {want} bytes), \
         {failed} failed, {unlisted} unlisted -> {}",
        csv.display(),
    );
    if failed + unlisted > 0 {
        return Err(format!("{} frame(s) not run", failed + unlisted).into());
    }
    Ok(())
}

fn probe_hef(
    path: &std::path::Path,
    images: Option<&std::path::Path>,
    csv: Option<&std::path::Path>,
    stats: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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
        // HailoRT dequantises `q -> qp_scale * (q - qp_zp)`, and derives
        // both from `limvals` -- which is therefore the representable range,
        // already measured and free of any assumption about the device dtype.
        // Deriving it again from a hardcoded 0..255 would add nothing when
        // right and be silently wrong on a non-uint8 output.
        println!(
            "          quant: qp_zp={} qp_scale={}  representable=[{}, {}]",
            info.qp_zp, info.qp_scale, info.limvals_min, info.limvals_max,
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
        OutputLayout::RawYolo26 {
            num_classes,
            scales,
        } => {
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
    if let Some(dir) = images {
        let csv = csv.ok_or("--images needs --csv <path>")?;
        run_images(&mut session, &layout, dir, csv, stats)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_default() {
        assert_eq!(parse_args(args(&[])).unwrap(), Args::default());
    }

    #[test]
    fn version_flag_sets_version() {
        let parsed = parse_args(args(&["--version"])).unwrap();
        assert!(parsed.version);
    }

    #[test]
    fn hef_images_csv_and_stats_are_parsed() {
        let parsed = parse_args(args(&[
            "--hef", "m.hef", "--images", "dir", "--csv", "out.csv", "--stats",
        ]))
        .unwrap();
        assert_eq!(parsed.hef, Some(PathBuf::from("m.hef")));
        assert_eq!(parsed.images, Some(PathBuf::from("dir")));
        assert_eq!(parsed.csv, Some(PathBuf::from("out.csv")));
        assert!(parsed.stats);
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse_args(args(&["--bogus"])).is_err());
    }

    #[test]
    fn stray_positional_is_rejected() {
        assert!(parse_args(args(&["not-a-flag"])).is_err());
    }

    #[test]
    fn hef_missing_value_is_rejected() {
        assert!(parse_args(args(&["--hef"])).is_err());
    }
}
