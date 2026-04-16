//! First-run detection and init tutorial trigger.
//!
//! On the very first invocation after installation, AlBDO writes a marker file
//! to `~/.albdo/.initialized` and re-launches itself with `albdo init` so the
//! user lands in the guided scaffold flow automatically.
//!
//! Subsequent invocations skip this entirely — the check is a single `Path::exists`
//! call so there is no measurable overhead on the hot path.

use std::fs;
use std::path::PathBuf;

/// Returns the path to the first-run marker: `~/.albdo/.initialized`.
///
/// Uses `USERPROFILE` (Windows) then `HOME` (Unix) — no external crate needed.
fn marker_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(home).join(".albdo").join(".initialized"))
}

/// Returns `true` if this is the first time `albdo` has run on this machine.
pub fn is_first_run() -> bool {
    marker_path().map(|p| !p.exists()).unwrap_or(false)
}

/// Writes the marker file so subsequent runs skip the init flow.
pub fn mark_initialized() {
    if let Some(path) = marker_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, "");
    }
}

/// If this is the first run, print a welcome line and delegate to `albdo init`.
///
/// Call this near the top of `run()`, before normal command dispatch:
///
/// ```rust,ignore
/// fn run(args: Vec<String>) -> Result<(), String> {
///     first_run::check_and_run_init();
///     // ... rest of dispatch
/// }
/// ```
pub fn check_and_run_init() {
    if !is_first_run() {
        return;
    }

    eprintln!();
    eprintln!("  Welcome to AlBDO.");
    eprintln!("  Running first-time setup...");
    eprintln!();

    // Write the marker before re-launching so a crash in `init` does not
    // cause an infinite first-run loop.
    mark_initialized();

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("albdo"));
    let status = std::process::Command::new(&exe)
        .arg("init")
        .status()
        .unwrap_or_else(|err| {
            eprintln!("  Warning: could not launch albdo init: {err}");
            // Return a fake success status so the binary exits cleanly.
            std::process::exit(0);
        });

    std::process::exit(status.code().unwrap_or(0));
}
