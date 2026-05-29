// Expose `NEXUS_BUILD_VERSION` to the crate as a `env!`-readable
// rustc env. Source of truth in order of precedence:
//   1. `NEXUS_RELEASE_VERSION` env var (set by .github/workflows/release.yml
//      to the release tag, e.g. `v0.1.27`) — leading `v`/`V` is stripped so
//      the UI can prepend its own `v` prefix without doubling it up.
//   2. `CARGO_PKG_VERSION` (`0.1.0` in this workspace until we bump
//      every crate's version) — used by `cargo build` / `cargo run`
//      during local dev so the dashboard's version pill still renders
//      *something*.
//
// The handler in `src/api.rs::health` reads `env!("NEXUS_BUILD_VERSION")`,
// so the value computed here is the only knob the UI / cloud sees.
fn main() {
    println!("cargo:rerun-if-env-changed=NEXUS_RELEASE_VERSION");

    let raw = std::env::var("NEXUS_RELEASE_VERSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);

    println!("cargo:rustc-env=NEXUS_BUILD_VERSION={}", stripped);
}
