//! Per-request server-compute timing — the honest number, in the terminal.
//!
//! ENDGAME doctrine: *"publish the harness, not the adjective."* When `albedo
//! dev` / `albedo serve` handle a request, this prints the one number that is
//! actually ALBEDO's to claim — the wall time the server spent turning a
//! request into a response (render → wire for a GET page, handler → effects for
//! a POST action). It is deliberately **not** a browser/network number: nothing
//! here measures DNS, connect, TLS, download, paint, or client hydration. Only
//! the span inside our process, which for Tier-A is precomputed bytes and lands
//! in the sub-millisecond / nanosecond band.
//!
//! Emitted only when the server is booted by the CLI (`with_request_timings`);
//! library embedders and the test harness stay silent by default.

use std::time::Duration;

// Palette — "Halation" (mirrors `src/bin/albedo/printer.rs`). Warm champagne
// gold on ink; the elapsed number is the hero, in pale cream-gold.
const ACCENT: u8 = 179; // champagne gold — the marker + method
const ACCENT_SOFT: u8 = 223; // pale gold / cream — the hero number
const MUTED: u8 = 245; // warm-neutral gray — the path

/// Width the plain path is padded to so the elapsed column lines up across
/// consecutive requests. Longer paths overflow gracefully (no truncation).
const PATH_COL: usize = 34;

/// Print one request-timing line: `▸ GET  /path                 142.7 µs`.
///
/// `method` is the HTTP verb, `path` the request path, `elapsed` the measured
/// server-compute span. Padding is applied to the *plain* strings before
/// colorizing — ANSI escapes carry zero display width, so styling first would
/// skew the column (the same bug the CLI restyle fixed in `printer.rs`).
pub fn print_request(method: &str, path: &str, elapsed: Duration) {
    let color = supports_color();

    let method_field = format!("{method:<4}");

    let path_pad = PATH_COL.saturating_sub(path.chars().count());
    let path_field = format!("{}{}", path, " ".repeat(path_pad));

    let elapsed_field = format_elapsed(elapsed);

    println!(
        "  {} {}  {}  {}",
        paint("▸", ACCENT, true, color),
        paint(&method_field, ACCENT, true, color),
        paint(&path_field, MUTED, false, color),
        paint(&elapsed_field, ACCENT_SOFT, true, color),
    );
}

/// Render a duration in the smallest ALBEDO-scale unit: nanoseconds below a
/// microsecond, microseconds below a millisecond, milliseconds above. Keeps the
/// output in the sub-millisecond band it belongs to instead of a stream of
/// `0.00 ms` lines.
fn format_elapsed(elapsed: Duration) -> String {
    let ns = elapsed.as_nanos();
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.1} µs", ns as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    }
}

fn paint(value: &str, color: u8, bold: bool, enabled: bool) -> String {
    if !enabled {
        return value.to_string();
    }
    if bold {
        format!("\u{1b}[1;38;5;{color}m{value}\u{1b}[0m")
    } else {
        format!("\u{1b}[38;5;{color}m{value}\u{1b}[0m")
    }
}

fn supports_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_nanoseconds_below_a_microsecond() {
        assert_eq!(format_elapsed(Duration::from_nanos(820)), "820 ns");
    }

    #[test]
    fn formats_microseconds_below_a_millisecond() {
        assert_eq!(format_elapsed(Duration::from_nanos(142_700)), "142.7 µs");
    }

    #[test]
    fn formats_milliseconds_above_one_ms() {
        assert_eq!(format_elapsed(Duration::from_micros(1_240)), "1.24 ms");
    }

    #[test]
    fn no_color_env_strips_ansi() {
        // Guard the escape-free path directly; env is process-global so we don't
        // toggle NO_COLOR here (it would race other tests) — assert the pure fn.
        assert_eq!(paint("GET", ACCENT, true, false), "GET");
        assert!(paint("GET", ACCENT, true, true).contains("\u{1b}["));
    }
}
