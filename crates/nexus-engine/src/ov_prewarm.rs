//! Build OpenVINO-routed ORT sessions once in a child process first.
//!
//! Intel's `libopencl-clang` has a heap use-after-free inside
//! `Compile()`, which OpenVINO's GPU plugin calls for every kernel not
//! already in the NEO compiler cache (`$HOME/.cache/neo_compiler_cache`).
//! Under the deploy unit's `MALLOC_ARENA_MAX=2` (BUG-036) another thread
//! can reclaim the freed chunk before the stale read, and the engine
//! segfaults on ~70% of cold compiles; with the cap unset it measured
//! 0 of 27 (BUG-269, #360).
//!
//! The cap has to stay — it is the engine's memory fix — so the compile
//! moves instead. Before the engine builds such a session itself,
//! [`spawn_and_wait`] re-runs this binary as `prewarm-session` without
//! the cap. The child compiles the kernels and fills the cache, and the
//! engine's own build of the same model then finds them there and never
//! calls `Compile()`. If the child crashes anyway, only the child dies.
//!
//! Registered with
//! [`nexus_inference::session_tuning::register_out_of_process_prewarm`]
//! on the serve path only; the child never registers it, so it cannot
//! recurse.

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nexus_inference::session_tuning::{self, PrewarmOutcome, SessionTuning};

/// The deploy unit's BUG-036 arena cap, and the trigger for the crash:
/// the child must not inherit it.
///
/// This is the only variable removed. The child's environment otherwise
/// matches the engine's, so it compiles identical kernels under identical
/// NEO cache keys — a child that compiled something else would warm the
/// cache for nothing. `MALLOC_MMAP_THRESHOLD_`, also set by the unit, was
/// measured harmless (0 of 10 cold compiles crashed) and stays.
const ARENA_CAP_ENV: &str = "MALLOC_ARENA_MAX";

/// How often [`wait_with_deadline`] checks on the child. Short against
/// a compile that takes seconds; long enough not to busy-wait.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The child invocation: `<exe> prewarm-session --model <model>
/// --ep <e1> --ep <e2> ...`, with the arena cap removed.
///
/// stdout is discarded because the child has nothing to report there.
/// stderr is inherited so that a child crash, or its error, lands in the
/// engine's journal next to the warning it causes.
fn command(exe: &Path, model: &Path, ep_priority: &[String]) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg("prewarm-session").arg("--model").arg(model);
    for ep in ep_priority {
        cmd.arg("--ep").arg(ep);
    }
    cmd.env_remove(ARENA_CAP_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}

/// Wait for `child` to exit, killing it at `timeout`.
///
/// Polled rather than blocking in `wait`, because a wedged compile never
/// exits and the whole point of the deadline is to stop waiting on it.
fn wait_with_deadline(mut child: Child, timeout: Duration) -> PrewarmOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return PrewarmOutcome::Built,
            Ok(Some(status)) => return PrewarmOutcome::Failed(describe(status)),
            Ok(None) => {}
            Err(e) => {
                kill_and_reap(&mut child);
                return PrewarmOutcome::Failed(format!("could not wait on the child: {e}"));
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            kill_and_reap(&mut child);
            return PrewarmOutcome::NeverReturned;
        }
        std::thread::sleep(left.min(POLL_INTERVAL));
    }
}

/// Kill `child` and wait for it, so a wedged compile neither keeps its
/// GPU context nor lingers as a zombie for the life of the engine.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// How a child that exited unsuccessfully exited. A crash is the case
/// worth naming: it says opencl-clang struck again, in the child.
fn describe(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    status.to_string()
}

/// Build the session for `model` against `ep_priority` in a child copy of
/// this binary, within `timeout`. The hook registered on the serve path.
pub fn spawn_and_wait(model: &Path, ep_priority: &[String], timeout: Duration) -> PrewarmOutcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return PrewarmOutcome::Failed(format!("could not locate this binary: {e}")),
    };
    match command(&exe, model, ep_priority).spawn() {
        Ok(child) => wait_with_deadline(child, timeout),
        Err(e) => PrewarmOutcome::Failed(format!("could not spawn {}: {e}", exe.display())),
    }
}

/// The `prewarm-session` subcommand: build the session and exit.
///
/// Goes through the same [`session_tuning::build_session`] as the engine
/// so the child compiles what the engine will. It deliberately does not
/// register the prewarm hook, so the child compiles in-process rather
/// than spawning a child of its own.
pub fn run_child(model: &Path, ep_priority: &[String]) -> anyhow::Result<()> {
    session_tuning::build_session(model, ep_priority, &SessionTuning::default())
        .map(drop)
        .map_err(|e| anyhow::anyhow!("prewarm {}: {e}", model.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child must differ from the engine by the arena cap and nothing
    /// else: anything more and it may compile different kernels than the
    /// engine will look up.
    #[test]
    fn the_child_drops_only_the_arena_cap_and_names_each_ep_in_order() {
        let chain = ["gpu".to_owned(), "cpu".to_owned()];
        let cmd = command(
            Path::new("/opt/nexus/bin/nexus-engine"),
            Path::new("/m.onnx"),
            &chain,
        );

        assert_eq!(cmd.get_program(), "/opt/nexus/bin/nexus-engine");
        let envs: Vec<_> = cmd.get_envs().collect();
        assert_eq!(
            envs,
            [(std::ffi::OsStr::new(ARENA_CAP_ENV), None)],
            "the child must remove the arena cap and change nothing else"
        );
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            [
                "prewarm-session",
                "--model",
                "/m.onnx",
                "--ep",
                "gpu",
                "--ep",
                "cpu"
            ],
        );
    }

    #[cfg(unix)]
    fn spawn(program: &str, args: &[&str]) -> Child {
        Command::new(program).args(args).spawn().expect("spawn")
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_exits_cleanly_is_built() {
        assert_eq!(
            wait_with_deadline(spawn("true", &[]), Duration::from_secs(30)),
            PrewarmOutcome::Built
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_exits_non_zero_failed() {
        assert!(matches!(
            wait_with_deadline(spawn("false", &[]), Duration::from_secs(30)),
            PrewarmOutcome::Failed(_)
        ));
    }

    /// The crash the prewarm exists to absorb: it must read as a failure
    /// the engine survives, naming the signal, not as a wedge.
    #[cfg(unix)]
    #[test]
    fn a_child_that_segfaults_failed_and_says_which_signal() {
        let got = wait_with_deadline(
            spawn("sh", &["-c", "kill -SEGV $$"]),
            Duration::from_secs(30),
        );
        assert_eq!(
            got,
            PrewarmOutcome::Failed("killed by signal 11".to_owned())
        );
    }

    /// A wedged compile must cost the engine its budget, not forever. The
    /// child is killed and reaped on the way out, by construction.
    #[cfg(unix)]
    #[test]
    fn a_child_that_never_exits_is_killed_at_the_deadline() {
        let started = Instant::now();
        let got = wait_with_deadline(spawn("sleep", &["30"]), Duration::from_millis(200));
        assert_eq!(got, PrewarmOutcome::NeverReturned);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}, so the deadline is not bounding the wait",
            started.elapsed()
        );
    }
}
