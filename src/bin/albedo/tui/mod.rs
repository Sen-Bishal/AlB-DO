//! The `albedo` terminal UI.
//!
//! ## What is and is not a dashboard
//!
//! Only the **long-running** commands take the screen: `albedo dev` and
//! `albedo serve` hold live state (routes, tier mix, requests, rebuilds) that a
//! developer watches for hours, and a redrawing surface beats a scrolling log
//! for that. Every one-shot command — `build`, `init`, `budget`, `files`,
//! `ship` — keeps streaming print.
//!
//! That split is deliberate and worth stating, because "use ratatui everywhere"
//! is the tempting version and it is wrong: an alternate-screen UI for
//! `albedo build` would erase its own output on exit, break
//! `albedo build | tee build.log`, and hand CI a screenful of escapes. Scrollback
//! and pipes are features. A dashboard that flashes and vanishes is a
//! regression wearing a nicer hat.
//!
//! ## Falling back
//!
//! [`available`] is checked before the terminal is ever touched. When stdout is
//! not a TTY (piped, redirected, CI), or `NO_COLOR` / `ALBEDO_NO_TUI` is set,
//! the caller runs its existing print path unchanged. The dashboard is an
//! enhancement layered on top of a CLI that still works without it.

use std::io::{self, IsTerminal, Stdout};

use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;

pub mod demo;
pub mod dev;
pub mod theme;

/// Whether a dashboard can be drawn at all.
///
/// `ALBEDO_NO_TUI` is the explicit escape hatch — for a user who simply prefers
/// the log, and for reproducing a report against the print path.
///
/// `RUST_LOG` opts out too, because the two cannot share a terminal: the
/// dashboard owns the alternate screen and repaints it every frame, so a
/// subscriber writing log lines to the same tty would be scribbled over
/// immediately — and it is the *log* the user asked for by setting the variable.
/// Asking for logs is therefore asking for the plain lane. (stderr does not
/// escape this: redirecting the stream elsewhere still leaves `RUST_LOG` set, so
/// `ALBEDO_NO_TUI= ` is not needed but the dashboard is still skipped — a
/// deliberate trade of one rare combination for a rule with no exceptions.)
pub fn available() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("ALBEDO_NO_TUI").is_none()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("RUST_LOG").is_none()
}

/// Owns the terminal's alternate screen and raw mode for as long as it lives.
///
/// The restore path runs on drop *and* from a panic hook installed at
/// construction. Without the hook a panic inside the draw loop would unwind past
/// the guard with raw mode still enabled, leaving the user with a shell that
/// does not echo — the single worst failure mode a TUI has, because it outlives
/// the process that caused it.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn new() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore();
            previous(info);
        }));

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore();
    }
}

/// Put the terminal back the way it was found. Idempotent and best-effort:
/// called from `Drop`, from the panic hook, and safe if the screen was never
/// entered.
fn restore() -> io::Result<()> {
    let _ = disable_raw_mode();
    execute!(io::stdout(), LeaveAlternateScreen)
}
