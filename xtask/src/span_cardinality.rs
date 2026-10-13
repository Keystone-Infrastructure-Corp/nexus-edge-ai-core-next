//! # span_cardinality
//!
//! Implements `cargo xtask check-span-cardinality`: a source-level gate on
//! the field keys that `tracing` events and spans are allowed to carry.
//!
//! ## Why
//!
//! Spans and events leave the process. The OTLP exporter in
//! `nexus-telemetry` ships spans to the cloud, and AGENTS.md rule 6 /
//! REPO_BOUNDARY R5b keep RTSP URLs, ONVIF credentials and sink endpoints
//! edge-resident. A field key that carries a secret, an RTSP URL, or a
//! per-request / per-frame identifier is therefore either a privacy leak
//! or a telemetry-cost bomb, and it is easy to add by accident.
//!
//! ## What gets flagged
//!
//! A key from [`FORBIDDEN_KEYS`] appearing as a field in:
//!
//! - `trace!` / `debug!` / `info!` / `warn!` / `error!` / `event!`
//! - `span!` / `trace_span!` / `debug_span!` / `info_span!` /
//!   `warn_span!` / `error_span!`
//! - `#[instrument(fields(...))]` / `#[tracing::instrument(fields(...))]`
//!
//! Recognised forms: `key = expr`, `key = %expr`, `key = ?expr`,
//! `"key" = expr`, and the shorthand `key,` / `%key,` / `?key,`. A dotted
//! key (`conn.token`) is matched on its last segment. An event macro also
//! accepts its fields as a braced group before the message
//! (`info!({token = %t}, "msg")`, `event!(Level::INFO, {token = %t}, "msg")`);
//! the group is split into fields and checked like bare ones. The span macros
//! do not accept that form (a braced argument there is a block expression),
//! so it is not read as fields. In an event macro
//! everything after the first string-literal argument is the message and
//! its format arguments, not fields, and is ignored; in a span macro that
//! string is the span name and fields follow it. Other macros (`format!`, `println!`,
//! `my_info!`) are never inspected.
//!
//! ## The value rule
//!
//! Field keys alone do not catch the credential-bearing camera URL: it can
//! be logged under another key (`rtsp_url = %cam.ingest.url`) or as a message
//! argument (`info!("connecting to {}", cam.ingest.url)`). So, inside the same
//! tracing macro forms, any argument whose expression contains `ingest.url`
//! is flagged too, under the key `ingest.url`: a field value, a message
//! format argument, or an argument of a nested non-tracing macro such as
//! `format!`. Text inside string literals and comments is not an expression
//! and is ignored.
//!
//! ## Known limits
//!
//! - The gate matches names and one value shape, not secrets. Another secret
//!   logged under a key that is not in [`FORBIDDEN_KEYS`] (a `pass` field, a
//!   bearer header under `auth`, a credential bound to a local first and then
//!   logged as `x = %local`) is not caught. Suffix matching was rejected on
//!   purpose: it would flag `profile_token`, an ONVIF profile name.
//! - `Span::record("key", ..)` calls and the implicit per-argument capture of
//!   a bare `#[instrument]` are not inspected.
//!
//! ## Escape hatch
//!
//! A site that genuinely needs a forbidden key carries a comment containing
//! `allow: span-cardinality <key>` followed by a specific reason, on the line
//! of the violation or on the line directly above it. The marker must name
//! the key it excuses (`ingest.url` for the value rule), so a bare marker
//! excuses nothing and a marker for one key never covers another:
//!
//! ```text
//! // allow: span-cardinality trace_id (one span per request, not per frame)
//! trace_id = %id,
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Marker that suppresses a violation on its own line or the next one, when
/// it is followed by the violation's key.
const ALLOW_MARKER: &str = "allow: span-cardinality";

/// The value rule: an argument expression containing this path is flagged,
/// whatever key it is logged under. A camera's `ingest.url` is stored with
/// `user:pass@` userinfo intact.
const VALUE_RULE_NEEDLE: &str = "ingest.url";

/// Field keys that must not appear on tracing macros, with the reason each
/// is here. Keep this list small: a key belongs only if it has a concrete
/// way to hurt this repo.
pub const FORBIDDEN_KEYS: &[&str] = &[
    // Secrets. A value logged under these names is a credential leak. The
    // engine holds cloud enrollment and OAuth tokens (nexus-storage-cloud),
    // TURN credentials (nexus-pipeline webrtc), and local-user passwords.
    // The engine's own diagnostics redactor (nexus-store diag_snapshot.rs
    // SECRET_MARKERS) treats the same names as secret.
    "password",
    "secret",
    "token",
    "api_key",
    "credential",
    "credentials",
    // Edge-resident locations (AGENTS.md rule 6, REPO_BOUNDARY R5b). A
    // camera's `ingest.url` is stored with `user:pass@` userinfo intact, and
    // `url` is the key it was being logged under; ONVIF device and sink
    // `endpoint`s are customer infrastructure that stays on the box.
    "url",
    "endpoint",
    // Per-request / per-frame identifiers: one fresh value per call, so
    // unbounded cardinality as an exported span attribute. `request_id`
    // is the per-RPC id in nexus-cloud-protocol; `frame_id` / `trace_id`
    // are minted per frame. Per-frame spans reach the cloud through the
    // TraceLayer, which captures every field as an attribute.
    "request_id",
    "frame_id",
    "trace_id",
];

/// One forbidden key found at a source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub file: PathBuf,
    /// 1-based line of the key.
    pub line: usize,
    pub key: String,
}

/// Macro names whose arguments are inspected.
const TRACING_MACROS: &[&str] = &[
    "trace",
    "debug",
    "info",
    "warn",
    "error",
    "event",
    "span",
    "trace_span",
    "debug_span",
    "info_span",
    "warn_span",
    "error_span",
];

/// Run the gate over `crates/**/*.rs` under `root`. Returns the process
/// exit code.
pub fn run(root: &Path) -> Result<i32> {
    let mut files = Vec::new();
    collect_rs(&root.join("crates"), &mut files)
        .with_context(|| format!("walking {}", root.join("crates").display()))?;
    files.sort();

    let mut violations = Vec::new();
    for file in &files {
        let src =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let rel = file.strip_prefix(root).unwrap_or(file);
        for (line, key) in scan_source(&src, FORBIDDEN_KEYS) {
            violations.push(Violation {
                file: rel.to_path_buf(),
                line,
                key,
            });
        }
    }

    for v in &violations {
        println!("{}:{}:{}", v.file.display(), v.line, v.key);
    }
    if violations.is_empty() {
        println!(
            "check-span-cardinality: ok ({} files, {} forbidden keys)",
            files.len(),
            FORBIDDEN_KEYS.len()
        );
        Ok(0)
    } else {
        eprintln!(
            "check-span-cardinality: {} violation(s). Remove the field, or add \
             `// {ALLOW_MARKER} <key> <reason>` naming the key, on the line or the \
             line above.",
            violations.len()
        );
        Ok(1)
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rs(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Index just past a string/char literal or comment starting at `i`, or
/// `None` if `i` does not start one.
fn skip_opaque(b: &[u8], i: usize) -> Option<usize> {
    let n = b.len();
    match b[i] {
        b'/' if i + 1 < n && b[i + 1] == b'/' => {
            let mut j = i;
            while j < n && b[j] != b'\n' {
                j += 1;
            }
            Some(j)
        }
        b'/' if i + 1 < n && b[i + 1] == b'*' => {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if b[j] == b'/' && j + 1 < n && b[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && j + 1 < n && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            Some(j)
        }
        b'"' => Some(skip_string(b, i + 1)),
        b'r' if (i == 0 || !is_ident_byte(b[i - 1])) => {
            // raw string r"..." / r#"..."#
            let mut j = i + 1;
            let mut hashes = 0;
            while j < n && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < n && b[j] == b'"' {
                j += 1;
                while j < n {
                    if b[j] == b'"' && b[j + 1..].iter().take(hashes).all(|&c| c == b'#') {
                        return Some((j + 1 + hashes).min(n));
                    }
                    j += 1;
                }
                Some(n)
            } else {
                None
            }
        }
        b'\'' => {
            // char literal vs lifetime
            if i + 1 < n && b[i + 1] == b'\\' {
                let mut j = i + 3;
                while j < n && b[j] != b'\'' {
                    j += 1;
                }
                Some((j + 1).min(n))
            } else if i + 2 < n && b[i + 2] == b'\'' {
                Some(i + 3)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `i` is just past an opening quote; returns the index past the closing one.
fn skip_string(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// `open` indexes `(`, `[` or `{`; returns the index of the matching close.
fn matching_close(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        if let Some(next) = skip_opaque(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split `b[start..end]` at top-level commas; returns `(arg_start, arg_end)`.
fn split_args(b: &[u8], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut arg_start = start;
    let mut i = start;
    while i < end {
        if let Some(next) = skip_opaque(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push((arg_start, i));
                arg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push((arg_start, end));
    out
}

/// Skip whitespace and comments.
fn skip_trivia(b: &[u8], mut i: usize, end: usize) -> usize {
    while i < end {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'/' && i + 1 < end && (b[i + 1] == b'/' || b[i + 1] == b'*') {
            i = skip_opaque(b, i).unwrap_or(i + 1);
        } else {
            break;
        }
    }
    i
}

enum ArgKind {
    /// A field with this key, starting at the given offset; the last item is
    /// the offset where its value expression starts (`None` for shorthand).
    Field(String, usize, Option<usize>),
    /// A string literal that is not a key: the message. Fields are over.
    Message,
    /// A braced field group, `{ a = 1, b = %c }`: the offsets of its
    /// contents. Only event macros accept one in the field position.
    Group(usize, usize),
    /// Anything else (`target: ..`, `Level::INFO`, `parent: ..`).
    Other,
}

fn classify_arg(src: &str, start: usize, end: usize) -> ArgKind {
    let b = src.as_bytes();
    let mut i = skip_trivia(b, start, end);
    if i >= end {
        return ArgKind::Other;
    }
    if b[i] == b'{' {
        // `{ fields }` is the whole argument, not the head of an expression
        // such as `{ a }.b`.
        return match matching_close(b, i) {
            Some(close) if skip_trivia(b, close + 1, end) >= end => ArgKind::Group(i + 1, close),
            _ => ArgKind::Other,
        };
    }
    let key_start = i;
    let mut value_start = None;

    let key: String = if b[i] == b'"' {
        let close = skip_string(b, i + 1);
        let raw = &src[i + 1..close.saturating_sub(1).max(i + 1)];
        i = skip_trivia(b, close, end);
        if i < end && b[i] == b'=' && b.get(i + 1) != Some(&b'=') {
            value_start = Some(i + 1);
            raw.to_string()
        } else {
            return ArgKind::Message;
        }
    } else {
        if b[i] == b'%' || b[i] == b'?' {
            i += 1;
            i = skip_trivia(b, i, end);
        }
        let name_start = i;
        while i < end && (is_ident_byte(b[i]) || b[i] == b'.') {
            i += 1;
        }
        if i == name_start || b[name_start].is_ascii_digit() {
            return ArgKind::Other;
        }
        let name = &src[name_start..i];
        let rest = skip_trivia(b, i, end);
        if rest < end {
            let is_assign = b[rest] == b'=' && !matches!(b.get(rest + 1), Some(b'=') | Some(b'>'));
            if !is_assign {
                return ArgKind::Other;
            }
            value_start = Some(rest + 1);
        }
        name.to_string()
    };

    // Dotted keys match on the last segment.
    let last = key.rsplit('.').next().unwrap_or(&key).to_string();
    ArgKind::Field(last, key_start, value_start)
}

fn line_of(src: &str, offset: usize) -> usize {
    src.as_bytes()[..offset]
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
        + 1
}

/// Whether `text` holds `allow: span-cardinality <key>`, with `key` ending at
/// a token boundary (so `url` does not match `url_base`).
fn marker_names_key(text: &str, key: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find(ALLOW_MARKER) {
        rest = &rest[at + ALLOW_MARKER.len()..];
        let named = rest.trim_start();
        if let Some(after) = named.strip_prefix(key) {
            if !after
                .bytes()
                .next()
                .is_some_and(|c| is_ident_byte(c) || c == b'.')
            {
                return true;
            }
        }
    }
    false
}

fn has_marker(lines: &[&str], line: usize, key: &str) -> bool {
    let idx = line - 1;
    marker_names_key(lines[idx], key) || (idx > 0 && marker_names_key(lines[idx - 1], key))
}

/// Offset of the first `ingest.url` in the code (not strings or comments) of
/// `b[start..end]`, on identifier boundaries.
fn find_value_needle(b: &[u8], start: usize, end: usize) -> Option<usize> {
    let needle = VALUE_RULE_NEEDLE.as_bytes();
    let mut i = start;
    while i < end {
        if let Some(next) = skip_opaque(b, i) {
            i = next;
            continue;
        }
        if b[i..end].starts_with(needle)
            && (i == 0 || !is_ident_byte(b[i - 1]))
            && !b.get(i + needle.len()).is_some_and(|&c| is_ident_byte(c))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Inspect the fields inside `b[start..end]` (the contents of a macro's
/// parentheses, a `fields(...)` group, or an event macro's braced field
/// group).
fn check_args(
    src: &str,
    lines: &[&str],
    keys: &[&str],
    (start, end): (usize, usize),
    fields_after_string: bool,
    out: &mut Vec<(usize, String)>,
) {
    let b = src.as_bytes();
    // In an event macro, arguments after the message are format arguments:
    // not fields, but still subject to the value rule.
    let mut past_message = false;
    for (a, z) in split_args(b, start, end) {
        let mut value_from = Some(a);
        if !past_message {
            match classify_arg(src, a, z) {
                ArgKind::Field(key, at, value_start) => {
                    if keys.contains(&key.as_str()) {
                        let line = line_of(src, at);
                        if !has_marker(lines, line, &key) {
                            out.push((line, key));
                        }
                    }
                    // Shorthand `cam.ingest.url` is already a `url` key.
                    value_from = value_start;
                }
                ArgKind::Group(inner_start, inner_end) => {
                    // A braced group is a field list, and only an event
                    // macro treats it as one: in a span macro it is a block
                    // expression, so it stays an ordinary argument.
                    if !fields_after_string {
                        check_args(src, lines, keys, (inner_start, inner_end), false, out);
                        value_from = None;
                    }
                }
                ArgKind::Message if fields_after_string => {}
                ArgKind::Message => past_message = true,
                ArgKind::Other => {}
            }
        }
        if let Some(from) = value_from {
            if let Some(at) = find_value_needle(b, from, z) {
                let line = line_of(src, at);
                if !has_marker(lines, line, VALUE_RULE_NEEDLE) {
                    out.push((line, VALUE_RULE_NEEDLE.to_string()));
                }
            }
        }
    }
}

/// Scan one source file; returns `(line, key)` for each unsuppressed
/// forbidden key.
pub fn scan_source(src: &str, keys: &[&str]) -> Vec<(usize, String)> {
    let b = src.as_bytes();
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = skip_opaque(b, i) {
            i = next;
            continue;
        }
        // Attribute: `#[ ... instrument( ... fields( ... ) ) ... ]`
        if b[i] == b'#' && b.get(i + 1) == Some(&b'[') {
            if let Some(close) = matching_close(b, i + 1) {
                scan_attribute(src, &lines, keys, i + 2, close, &mut out);
                i = close + 1;
                continue;
            }
        }
        // Macro invocation: `name!(` where `name` is not a suffix of a
        // longer identifier.
        if is_ident_byte(b[i]) && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i;
            while j < b.len() && is_ident_byte(b[j]) {
                j += 1;
            }
            let name = &src[i..j];
            if TRACING_MACROS.contains(&name) && b.get(j) == Some(&b'!') {
                let open = skip_trivia(b, j + 1, b.len());
                if matches!(b.get(open), Some(b'(') | Some(b'[') | Some(b'{')) {
                    if let Some(close) = matching_close(b, open) {
                        let is_span = name.ends_with("span") && name != "event";
                        check_args(src, &lines, keys, (open + 1, close), is_span, &mut out);
                        // Nested macros in the arguments are scanned too.
                        i = open + 1;
                        continue;
                    }
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    // A nested tracing macro is reached from its parent's arguments too.
    out.sort();
    out.dedup();
    out
}

fn scan_attribute(
    src: &str,
    lines: &[&str],
    keys: &[&str],
    start: usize,
    end: usize,
    out: &mut Vec<(usize, String)>,
) {
    let b = src.as_bytes();
    let mut has_instrument = false;
    let mut i = start;
    let mut fields_groups = Vec::new();
    while i < end {
        if let Some(next) = skip_opaque(b, i) {
            i = next;
            continue;
        }
        if is_ident_byte(b[i]) && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i;
            while j < end && is_ident_byte(b[j]) {
                j += 1;
            }
            match &src[i..j] {
                "instrument" => has_instrument = true,
                "fields" => {
                    let open = skip_trivia(b, j, end);
                    if b.get(open) == Some(&b'(') {
                        if let Some(close) = matching_close(b, open) {
                            fields_groups.push((open + 1, close));
                            i = close + 1;
                            continue;
                        }
                    }
                }
                _ => {}
            }
            i = j;
            continue;
        }
        i += 1;
    }
    if has_instrument {
        for group in fields_groups {
            check_args(src, lines, keys, group, false, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &["token", "trace_id", "rtsp_url"];

    fn scan(src: &str) -> Vec<(usize, String)> {
        scan_source(src, KEYS)
    }

    fn keys_of(src: &str) -> Vec<String> {
        scan(src).into_iter().map(|(_, k)| k).collect()
    }

    #[test]
    fn event_macros_flag_every_level() {
        for m in ["trace", "debug", "info", "warn", "error", "event"] {
            let src = format!("fn f() {{ {m}!(token = %t, \"msg\"); }}");
            assert_eq!(keys_of(&src), ["token"], "{m}!");
        }
    }

    #[test]
    fn span_macros_flag_every_level() {
        for m in [
            "span",
            "trace_span",
            "debug_span",
            "info_span",
            "warn_span",
            "error_span",
        ] {
            let src = format!("fn f() {{ let _s = {m}!(\"x\", trace_id = %id); }}");
            assert_eq!(keys_of(&src), ["trace_id"], "{m}!");
        }
    }

    #[test]
    fn path_qualified_macro_is_matched() {
        assert_eq!(keys_of("tracing::info!(token = 1);"), ["token"]);
        assert_eq!(
            keys_of("tracing::info_span!(\"x\", trace_id = 1);"),
            ["trace_id"]
        );
    }

    #[test]
    fn instrument_fields_are_flagged() {
        let src = "#[instrument(skip_all, fields(camera_id = %c, trace_id = %t))]\nfn f() {}";
        assert_eq!(scan(src), [(1, "trace_id".to_string())]);
        let src = "#[tracing::instrument(level = \"debug\", fields(token))]\nfn f() {}";
        assert_eq!(keys_of(src), ["token"]);
    }

    #[test]
    fn non_instrument_attribute_with_fields_is_ignored() {
        assert!(scan("#[derive(Foo)]\n#[foo(fields(token = 1))]\nstruct S;").is_empty());
    }

    #[test]
    fn shorthand_and_sigil_forms() {
        assert_eq!(keys_of("info!(token, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!(%token, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!(?token, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!(token = ?t, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!(\"token\" = 1, \"m\");"), ["token"]);
    }

    #[test]
    fn dotted_key_matches_on_last_segment() {
        assert_eq!(keys_of("info!(conn.token = 1, \"m\");"), ["token"]);
    }

    #[test]
    fn multiline_reports_key_line() {
        let src = "info!(\n    camera_id,\n    trace_id = %t,\n    \"m\",\n);";
        assert_eq!(scan(src), [(3, "trace_id".to_string())]);
    }

    #[test]
    fn allow_marker_on_same_line() {
        let src =
            "info!(\n    trace_id = %t, // allow: span-cardinality trace_id one per run\n    \"m\",\n);";
        assert!(scan(src).is_empty());
    }

    #[test]
    fn allow_marker_on_line_above() {
        let src = "info!(\n    // allow: span-cardinality trace_id one per run\n    trace_id = %t,\n    \"m\",\n);";
        assert!(scan(src).is_empty());
    }

    #[test]
    fn allow_marker_two_lines_up_does_not_suppress() {
        let src = "// allow: span-cardinality trace_id too far\n\ninfo!(trace_id = %t, \"m\");";
        assert_eq!(keys_of(src), ["trace_id"]);
    }

    #[test]
    fn allow_marker_covers_only_its_key() {
        let src = "info!(\n    // allow: span-cardinality trace_id ok\n    trace_id = %t,\n    token = %k,\n    \"m\",\n);";
        assert_eq!(scan(src), [(4, "token".to_string())]);
    }

    #[test]
    fn allow_marker_must_name_the_key() {
        // A bare marker excuses nothing.
        let src = "info!(\n    // allow: span-cardinality one per run\n    trace_id = %t,\n    \"m\",\n);";
        assert_eq!(keys_of(src), ["trace_id"]);
        // A marker for another key excuses nothing.
        let src = "// allow: span-cardinality token reason\ninfo!(trace_id = %t, \"m\");";
        assert_eq!(keys_of(src), ["trace_id"]);
        // A key that merely starts with the named one does not match it.
        let src = "// allow: span-cardinality trace_id_v2 reason\ninfo!(trace_id = %t, \"m\");";
        assert_eq!(keys_of(src), ["trace_id"]);
        // The key may be followed by punctuation.
        let src = "// allow: span-cardinality trace_id: reason\ninfo!(trace_id = %t, \"m\");";
        assert!(scan(src).is_empty());
    }

    #[test]
    fn value_rule_flags_camera_url_under_another_key() {
        assert_eq!(
            scan("info!(source = %cam.ingest.url, \"x\");"),
            [(1, "ingest.url".to_string())]
        );
        assert_eq!(
            keys_of("warn_span!(\"x\", src = ?cfg.ingest.url.clone());"),
            ["ingest.url"]
        );
        assert_eq!(
            keys_of("#[instrument(fields(src = %cam.ingest.url))]\nfn f() {}"),
            ["ingest.url"]
        );
    }

    #[test]
    fn value_rule_flags_message_format_args() {
        assert_eq!(
            scan("info!(\"connecting to {}\", cam.ingest.url);"),
            [(1, "ingest.url".to_string())]
        );
        assert_eq!(
            keys_of("error!(camera_id = %id, \"failed {}\", format!(\"{}\", cam.ingest.url));"),
            ["ingest.url"]
        );
        assert_eq!(
            keys_of("info!(\"connecting to {u}\", u = cam.ingest.url);"),
            ["ingest.url"]
        );
    }

    #[test]
    fn value_rule_reports_the_line_and_honours_a_named_marker() {
        let src = "info!(\n    camera_id,\n    src = %cam.ingest.url,\n    \"m\",\n);";
        assert_eq!(scan(src), [(3, "ingest.url".to_string())]);
        let src = "info!(\n    // allow: span-cardinality ingest.url redacted upstream\n    src = %cam.ingest.url,\n    \"m\",\n);";
        assert!(scan(src).is_empty());
        // The `url` key marker does not cover the value rule.
        let src = "info!(\n    // allow: span-cardinality url reason\n    src = %cam.ingest.url,\n    \"m\",\n);";
        assert_eq!(keys_of(src), ["ingest.url"]);
    }

    #[test]
    fn value_rule_is_precise() {
        // Only the tracing macros are inspected.
        assert!(scan("let s = format!(\"{}\", cam.ingest.url);").is_empty());
        assert!(scan("println!(\"{}\", cam.ingest.url);").is_empty());
        assert!(scan("my_info!(src = cam.ingest.url);").is_empty());
        // Not a different path, and not text in a string or comment.
        assert!(scan("info!(src = %cam.my_ingest.url, \"m\");").is_empty());
        assert!(scan("info!(src = %cam.ingest.url_kind, \"m\");").is_empty());
        assert!(scan("info!(\"see ingest.url\", camera_id);").is_empty());
        assert!(scan("info!(camera_id, /* ingest.url */ \"m\");").is_empty());
        // An unrelated ingest field is fine, and the profile token is not
        // a secret key (no suffix matching).
        assert!(scan("info!(mode = %cam.ingest.mode, profile_token = %p, \"m\");").is_empty());
    }

    #[test]
    fn non_tracing_macros_do_not_match() {
        assert!(scan("let s = format!(\"{}\", token);").is_empty());
        assert!(scan("println!(\"x\", token = 1);").is_empty());
        assert!(scan("my_info!(token = 1);").is_empty());
        assert!(scan("let v = vec![token, trace_id];").is_empty());
    }

    #[test]
    fn message_format_args_are_not_fields() {
        assert!(scan("info!(\"got {}\", token);").is_empty());
        assert!(scan("info!(camera_id, \"got {token}\", token = 1);").is_empty());
    }

    #[test]
    fn keys_inside_strings_and_comments_are_ignored() {
        assert!(scan("// info!(token = 1);\nlet s = \"info!(token = 1)\";").is_empty());
        assert!(scan("/* info!(token = 1) */").is_empty());
        assert!(scan("let s = r#\"info!(token = 1)\"#;").is_empty());
    }

    #[test]
    fn value_expression_is_not_a_key() {
        // `token` is only mentioned in the value of another field.
        assert!(scan("info!(camera_id = %token.id, \"m\");").is_empty());
        assert!(scan("info!(n = f(token, 1), \"m\");").is_empty());
    }

    #[test]
    fn target_and_level_args_are_skipped() {
        assert_eq!(
            keys_of("event!(target: \"a\", Level::INFO, token = 1, \"m\");"),
            ["token"]
        );
    }

    #[test]
    fn nested_macro_in_args_is_scanned() {
        assert_eq!(
            keys_of("let s = info_span!(\"x\", n = { debug!(token = 1); 2 });"),
            ["token"]
        );
    }

    #[test]
    fn braced_field_group_in_a_level_macro_is_flagged() {
        assert_eq!(
            keys_of("tracing::info!({token = %secret}, \"connecting\");"),
            ["token"]
        );
        for m in ["trace", "debug", "info", "warn", "error"] {
            let src = format!("fn f() {{ {m}!({{token = %t}}, \"msg\"); }}");
            assert_eq!(keys_of(&src), ["token"], "{m}!");
        }
    }

    #[test]
    fn braced_field_group_in_event_is_flagged() {
        // Both reviewer cases, against the real key list.
        let hits = |src: &str| -> Vec<String> {
            scan_source(src, FORBIDDEN_KEYS)
                .into_iter()
                .map(|(_, k)| k)
                .collect()
        };
        assert_eq!(
            hits("tracing::info!({token = %secret}, \"connecting\");"),
            ["token"]
        );
        assert_eq!(
            hits("tracing::event!(tracing::Level::INFO, {password = %secret}, \"connecting\");"),
            ["password"]
        );
        assert_eq!(
            keys_of("event!(target: \"a\", Level::INFO, {n = 1, trace_id = %t}, \"m\");"),
            ["trace_id"]
        );
    }

    #[test]
    fn braced_field_group_covers_key_forms_and_reports_the_key_line() {
        assert_eq!(keys_of("info!({\"token\" = 1, n = 2}, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!({n = 1, ?token}, \"m\");"), ["token"]);
        assert_eq!(keys_of("info!({n = 1, c.token}, \"m\");"), ["token"]);
        assert_eq!(
            scan("info!({\n    n = 1,\n    token = %t,\n}, \"m\");"),
            [(3, "token".to_string())]
        );
    }

    #[test]
    fn braced_field_group_gets_the_value_rule() {
        assert_eq!(
            keys_of("info!({rtsp_url = %cam.ingest.url}, \"m\");"),
            ["ingest.url", "rtsp_url"]
        );
        assert_eq!(
            keys_of("info!({n = %cam.ingest.url}, \"m\");"),
            ["ingest.url"]
        );
    }

    #[test]
    fn braced_field_group_honours_the_escape_hatch() {
        let src = "info!({\n    // allow: span-cardinality token (test fixture)\n    token = 1,\n}, \"m\");";
        assert!(scan(src).is_empty());
        let src = "info!({\n    // allow: span-cardinality trace_id (other key)\n    token = 1,\n}, \"m\");";
        assert_eq!(keys_of(src), ["token"]);
    }

    #[test]
    fn braced_field_group_with_only_allowed_fields_is_clean() {
        assert!(scan("tracing::info!({camera_id = %c, n = 1}, \"m\");").is_empty());
        assert!(scan("event!(Level::INFO, {camera_id = %c, ?n}, \"m\");").is_empty());
        assert!(scan("info!({}, \"m\");").is_empty());
    }

    #[test]
    fn braces_after_the_message_are_not_fields() {
        assert!(scan("info!(\"m {}\", { token = 1 });").is_empty());
        assert!(scan("info!(n = 1, \"m\", { token });").is_empty());
    }

    #[test]
    fn braced_expression_is_not_a_field_group() {
        // `{ token }.len()` is an expression, not a group of fields.
        assert!(scan("info!(n = { token }.len(), \"m\");").is_empty());
        assert!(scan("info!({ token }.len(), \"m\");").is_empty());
    }

    #[test]
    fn span_macros_do_not_take_braced_field_groups() {
        // tracing 0.1.44 rejects `{ k = v }` after a span name (it is parsed
        // as a block expression), so there is no group to read as fields.
        assert!(scan("let _s = info_span!(\"x\", { token = 1 });").is_empty());
        assert!(scan("let _s = span!(Level::INFO, \"x\", { token = 1 });").is_empty());
        // Fields outside the braces are still read.
        assert_eq!(
            keys_of("let _s = info_span!(\"x\", { 1 }, token = 1);"),
            ["token"]
        );
    }

    #[test]
    fn real_keys_are_not_trivially_empty() {
        assert!(FORBIDDEN_KEYS.contains(&"trace_id"));
        assert!(FORBIDDEN_KEYS.contains(&"password"));
    }
}
