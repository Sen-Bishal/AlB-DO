//! Regression cover for the CLI's first-run path.
//!
//! The bug these pin: `check_and_run_init` re-launched the binary as a bare
//! `albdo init` and exited with that child's status, *before* argument
//! dispatch. Since `init` requires a project name, the first command any new
//! user ran on a fresh machine failed with "missing project name" and their
//! own arguments were silently discarded. It only reproduced on a machine with
//! no `~/.albdo/.initialized` marker, which is why an established dev box never
//! saw it — and why these tests point `HOME`/`USERPROFILE` at a temp dir to
//! manufacture that fresh-machine state on every run.
//!
//! Both the real binary and a real scaffold are exercised: the failure was in
//! process launch and argument plumbing, so nothing short of spawning the
//! actual executable would have caught it.

use std::path::Path;
use std::process::Command;

/// A `Command` for the real `albedo` binary, run as if on a brand-new machine:
/// `cwd` is an empty dir and the home (both spellings) is an empty dir, so the
/// first-run marker is guaranteed absent.
fn albedo_on_a_fresh_machine(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    // USERPROFILE is read first on Windows, HOME on Unix — set both so the
    // test manufactures the same fresh-machine state on either platform.
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd
}

/// THE regression. On a fresh machine `albedo init <name>` must scaffold
/// `<name>` — not discard the name and fail.
#[test]
fn the_first_command_on_a_fresh_machine_still_runs_what_the_user_typed() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");

    let out = albedo_on_a_fresh_machine(home.path(), work.path())
        .args(["init", "my-app"])
        .output()
        .expect("the albedo binary runs");

    assert!(
        out.status.success(),
        "the first command on a fresh machine must succeed, got {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The user's project name has to survive the first-run path. Assert on a
    // real scaffolded file: a bare `mkdir` would satisfy `is_dir` even if
    // scaffolding never happened.
    assert!(
        work.path().join("my-app").join("package.json").is_file(),
        "`init my-app` must scaffold my-app/ — the project name was dropped"
    );

    // And the greeting must actually have fired, or this test would be passing
    // for the trivial reason that it never exercised the first-run path.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Welcome"),
        "precondition: this must BE a first run, but no welcome was printed.\nstderr: {stderr}"
    );
}

/// First-run must not swallow non-`init` commands either — the hijack applied
/// to whatever the user typed.
#[test]
fn the_first_command_on_a_fresh_machine_can_be_something_other_than_init() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");

    let out = albedo_on_a_fresh_machine(home.path(), work.path())
        .arg("help")
        .output()
        .expect("the albedo binary runs");

    assert!(out.status.success(), "`albedo help` must work on a fresh machine");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("init"),
        "`help` must print the help text, not be replaced by first-run.\nstdout: {stdout}"
    );
}

/// The welcome is one-time: it greets on the first run and stays quiet after,
/// which is the whole reason the marker exists.
#[test]
fn the_welcome_is_printed_once_and_not_again() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");

    let first = albedo_on_a_fresh_machine(home.path(), work.path())
        .arg("help")
        .output()
        .expect("the albedo binary runs");
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("Welcome"),
        "the first run must greet"
    );

    // Same home => the marker written by the first run is now present.
    let second = albedo_on_a_fresh_machine(home.path(), work.path())
        .arg("help")
        .output()
        .expect("the albedo binary runs");
    assert!(
        !String::from_utf8_lossy(&second.stderr).contains("Welcome"),
        "the second run must not greet again"
    );
    assert!(second.status.success());
}

/// A failing command must report its own failure — the old path laundered the
/// child's exit code and could report success for a failed run.
#[test]
fn a_bad_command_on_a_fresh_machine_still_reports_failure() {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");

    let out = albedo_on_a_fresh_machine(home.path(), work.path())
        .arg("no-such-command")
        .output()
        .expect("the albedo binary runs");

    assert!(
        !out.status.success(),
        "an unknown command must exit non-zero even on the first run"
    );
}
