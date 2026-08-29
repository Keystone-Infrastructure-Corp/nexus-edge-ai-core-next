//! Channel-pointer invariants for new installs (ADR-080).
//!
//! These assert over shell and workflow YAML rather than Rust, following
//! `remote_support_privileges.rs`, because that is where the behaviour lives:
//! what a fresh box installs is decided by a URL in one script and two
//! `gh release upload` calls in two different workflows. Every failure mode
//! here is silent. A promotion that forgets the pointer updates the fleet and
//! leaves new installs on the previous build. An asset renamed on one side of
//! the contract 404s only on an operator's box, never in CI. And a second copy
//! of the channel calculation drifts from the first without anything failing.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ always has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Both paths that put a build on a channel must move the pointer.
///
/// `release.yml` publishes straight to a channel; `promote-release-channel.yml`
/// moves an already-published build onto the next one, and is the reviewed way
/// a soaked build reaches `stable` (ADR-071). The cloud registration in the
/// promotion workflow only moves enrolled cores — if it does not also re-point
/// the pointer, the promoted build never reaches a single new install and
/// nothing reports it.
#[test]
fn every_workflow_that_sets_a_channel_repoints_the_pointer() {
    for wf in [
        ".github/workflows/release.yml",
        ".github/workflows/promote-release-channel.yml",
    ] {
        let body = read(wf);
        assert!(
            body.contains("gh release upload \"$CHANNEL\""),
            "{wf} decides a release channel but never uploads to the channel \
             pointer. New installs resolve their version from that pointer \
             (ADR-080), so a channel move that skips it is invisible to every \
             fresh box."
        );
    }
}

/// The pointer asset name is a contract between three files.
///
/// `bootstrap.sh` fetches it by name; both workflows publish it by name. A
/// rename on either side is a 404 on an operator's box during install and
/// green in CI.
#[test]
fn bootstrap_and_workflows_agree_on_the_pointer_asset() {
    let bootstrap = read("scripts/bootstrap.sh");
    assert!(
        bootstrap.contains("/releases/download/${CHANNEL}/VERSION"),
        "bootstrap.sh must resolve a channel by reading the pointer's VERSION asset"
    );

    for wf in [
        ".github/workflows/release.yml",
        ".github/workflows/promote-release-channel.yml",
    ] {
        let body = read(wf);
        assert!(
            body.contains("printf '%s\\n' \"$TAG\" > VERSION"),
            "{wf} must write the tag it is publishing into a file named VERSION \
             — that exact asset name is what bootstrap.sh fetches"
        );
    }
}

/// The pointer must serve the installer, not just the version.
///
/// Every documented one-liner curls `bootstrap.sh` from the channel pointer, so
/// a pointer carrying only `VERSION` 404s the install before it starts. Moving
/// one asset and not the other is invisible in CI and total for the operator.
#[test]
fn the_channel_pointer_carries_bootstrap_itself() {
    for wf in [
        ".github/workflows/release.yml",
        ".github/workflows/promote-release-channel.yml",
    ] {
        let body = read(wf);
        let upload = body
            .split_once("gh release upload \"$CHANNEL\"")
            .expect("guarded by every_workflow_that_sets_a_channel_repoints_the_pointer")
            .1;
        // The argument list ends at the step's closing echo.
        let args = upload
            .split_once("\n          echo")
            .map_or(upload, |(before, _)| before);

        assert!(
            args.contains("VERSION"),
            "{wf} uploads to the channel pointer without VERSION, which is the \
             asset bootstrap.sh resolves a channel from"
        );
        assert!(
            args.contains("bootstrap.sh"),
            "{wf} uploads to the channel pointer without bootstrap.sh. Every \
             documented install one-liner curls it from the pointer, so the \
             pointer would resolve a version nobody can reach."
        );
    }
}

/// A no-flag install must resolve the stable channel, not GitHub's "latest".
///
/// GitHub's `/releases/latest` answers "newest tag not flagged prerelease",
/// which is not the channel the release workflow published on — a full release
/// routed to `beta` is withheld from the fleet and still served to new boxes.
/// That is the defect ADR-080 exists to close, and it would come back silently.
#[test]
fn bootstrap_defaults_to_the_stable_channel() {
    let bootstrap = read("scripts/bootstrap.sh");

    assert!(
        bootstrap.contains(r#"CHANNEL="${CHANNEL:-stable}""#),
        "bootstrap.sh must default to the stable channel when neither --channel \
         nor --version is given"
    );
    assert!(
        !bootstrap.contains("releases/latest"),
        "bootstrap.sh must not resolve a version through GitHub's \
         /releases/latest — it reports the prerelease flag, not the channel \
         the release was published on (ADR-080)"
    );
}

/// The channel is calculated once, in `resolve-channel`.
///
/// Two consumers read it: the pointer step and the cloud registration. A second
/// copy of the precedence rules would let a build be registered on one channel
/// and served on another, which is the same class of split ADR-080 closes.
#[test]
fn release_workflow_calculates_the_channel_exactly_once() {
    let body = read(".github/workflows/release.yml");
    let mappings = body.matches(r#"channel="beta""#).count();
    assert_eq!(
        mappings, 1,
        "the prerelease -> beta mapping appears {mappings} times in release.yml; \
         it belongs only in the `resolve-channel` job, whose output every other \
         job reads. A second copy drifts from the first silently."
    );
}

/// The pipeline must never treat a channel pointer as a release to build.
///
/// `stable`/`beta`/`dev` are now real tags in this repo, so a `workflow_dispatch`
/// can be handed one — and dispatch is exempt from the rule that keeps
/// GITHUB_TOKEN-raised events from starting runs. The version stamp writes
/// `version = "stable"` into `Cargo.toml` without erroring, so the run dies
/// later and somewhere far less obvious. Every job carrying the `models-`
/// guard needs the pointer exclusion for the same reason.
#[test]
fn the_release_pipeline_skips_channel_pointer_tags() {
    let body = read(".github/workflows/release.yml");

    let model_guards = body.matches("'models-')").count();
    assert!(
        model_guards > 0,
        "expected release.yml to guard against models-* tags; the guard shape \
         changed and this test needs updating with it"
    );

    let pointer_guards = body
        .matches(r#"!contains(fromJSON('["stable", "beta", "dev"]')"#)
        .count();
    assert_eq!(
        pointer_guards, model_guards,
        "{model_guards} job(s) skip `models-*` tags but only {pointer_guards} \
         also skip the `stable`/`beta`/`dev` channel pointers. A job guarded \
         against one and not the other will try to build a release out of a \
         pointer tag."
    );
}
