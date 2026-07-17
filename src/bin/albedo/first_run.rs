//! First-run detection.
//!
//! On the very first invocation after installation AlBDO prints a one-time
//! welcome and writes a marker file to `~/.albdo/.initialized`. Every later
//! invocation skips this entirely — the check is a single `Path::exists` call,
//! so there is no measurable overhead on the hot path.
//!
//! **First-run greets; it does not dispatch.** An earlier version re-launched
//! the binary as `albdo init` and exited with the child's status. That
//! discarded whatever the user had actually typed: on a fresh machine
//! `albdo init my-app` became a bare `albdo init`, and since `init` requires a
//! project name it failed outright — so the first command a new user ever ran
//! always failed. The re-launch was written when `init` took no arguments and
//! scaffolded into the current directory; it outlived that signature. Nothing
//! here may run a command on the user's behalf.

use std::fs;
use std::path::{Path, PathBuf};

/// The marker location relative to a home directory: `<home>/.albdo/.initialized`.
///
/// Taking `home` as an argument rather than reading the environment is what
/// makes the marker logic testable against a temp dir instead of the real
/// `~` of whoever runs the suite.
fn marker_path_in(home: &Path) -> PathBuf {
    home.join(".albdo").join(".initialized")
}

/// Resolves the user's home via `USERPROFILE` (Windows) then `HOME` (Unix) —
/// no external crate needed. `None` when neither is set.
fn home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

/// Returns `true` when the marker is absent from `home`.
fn is_first_run_in(home: &Path) -> bool {
    !marker_path_in(home).exists()
}

/// Writes the marker so later runs stay quiet. Idempotent, and best-effort by
/// design: the worst a failure here can cost is a repeated greeting, which is
/// never worth failing a user's command over.
fn mark_initialized_in(home: &Path) {
    let path = marker_path_in(home);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, "");
}

/// Prints a one-time welcome if this is the first run on this machine, and
/// records that it did. Returns `true` if it greeted.
///
/// **Always returns.** The caller's command dispatch must proceed untouched —
/// see the module docs for the bug that rule exists to prevent.
///
/// With no resolvable home there is nowhere to keep the marker, so stay silent
/// rather than greet on every single invocation.
pub fn welcome_on_first_run() -> bool {
    let Some(home) = home_dir() else {
        return false;
    };
    if !is_first_run_in(&home) {
        return false;
    }

    // Mark before printing so an interrupt mid-greeting cannot loop the
    // welcome forever.
    mark_initialized_in(&home);

    eprintln!();
    eprintln!("  Welcome to AlBDO.");
    eprintln!();

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_lives_under_dot_albdo_in_the_home_dir() {
        let home = Path::new("/somewhere/home");
        assert_eq!(
            marker_path_in(home),
            Path::new("/somewhere/home").join(".albdo").join(".initialized")
        );
    }

    #[test]
    fn a_home_without_the_marker_is_a_first_run() {
        let home = tempfile::tempdir().expect("temp home");
        assert!(is_first_run_in(home.path()));
    }

    #[test]
    fn marking_initialized_creates_the_marker_and_ends_the_first_run() {
        let home = tempfile::tempdir().expect("temp home");
        assert!(is_first_run_in(home.path()), "precondition: fresh home");

        mark_initialized_in(home.path());

        assert!(marker_path_in(home.path()).is_file());
        assert!(!is_first_run_in(home.path()), "the marker must end the first run");
    }

    #[test]
    fn marking_initialized_is_idempotent() {
        let home = tempfile::tempdir().expect("temp home");
        mark_initialized_in(home.path());
        mark_initialized_in(home.path());
        assert!(!is_first_run_in(home.path()));
    }

    /// The marker's parent (`.albdo/`) does not exist on a fresh machine —
    /// writing it must create the directory rather than silently fail and
    /// re-greet forever.
    #[test]
    fn marking_initialized_creates_the_missing_parent_directory() {
        let home = tempfile::tempdir().expect("temp home");
        assert!(!home.path().join(".albdo").exists(), "precondition: no .albdo yet");

        mark_initialized_in(home.path());

        assert!(home.path().join(".albdo").is_dir());
    }
}
