//! The live dashboard behind `albedo dev` / `albedo serve`.
//!
//! Everything on screen is a signal the CLI already had and used to scroll past:
//! the tier mix from the build's own `TierReport`, request timings from the
//! server's one timing choke point, rebuild results from the watch loop. Nothing
//! here is invented, and nothing is sampled — if a number appears, something
//! measured it.
//!
//! The layout encodes the same claim the palette does. **Tiers sit above
//! requests** because the tier mix is what ALBEDO decided, and the request
//! timings are the consequence; a developer reading top-to-bottom reads cause
//! then effect.

use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::types::TierReport;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Sparkline, Table,
};
use ratatui::Frame;

use super::theme;
use super::TerminalGuard;

/// The version line. Carried here rather than from `CARGO_PKG_VERSION` because
/// it is a *product* label, not a crate version — the two move independently.
pub const VERSION_LABEL: &str = "V : BETA 1";

/// STRATEGY's solar ladder, shown as a quiet footer to the wordmark. Free
/// framework at Sol; every rung above it sells hosted compute, never the
/// binary.
pub const TIER_LADDER: [&str; 4] = ["SOL", "EQUINOX", "UMBRA", "PERSEPHONE"];

/// How many rows each scrolling panel retains. Bounded on purpose: an unbounded
/// log in a long-running dev session is a slow memory leak that only shows up
/// after the developer has stopped watching.
const HISTORY: usize = 500;

/// Something worth putting on screen, pushed in from the CLI and the server.
#[derive(Debug, Clone)]
pub enum DashEvent {
    Request {
        method: String,
        path: String,
        elapsed: Duration,
    },
    Reloaded {
        millis: f64,
    },
    BuildFailed {
        message: String,
    },
    Note {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dev,
    Serve,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Dev => "dev",
            Mode::Serve => "serve",
        }
    }
}

struct RequestRow {
    method: String,
    path: String,
    elapsed: Duration,
}

struct EventRow {
    at: Instant,
    kind: EventKind,
    message: String,
}

#[derive(Clone, Copy)]
enum EventKind {
    Ok,
    Fail,
    Note,
}

pub struct Dashboard {
    mode: Mode,
    url: String,
    project: String,
    report: Option<TierReport>,
    started: Instant,
    requests: VecDeque<RequestRow>,
    events: VecDeque<EventRow>,
    request_count: u64,
}

impl Dashboard {
    pub fn new(mode: Mode, url: String, project: String, report: Option<TierReport>) -> Self {
        Self {
            mode,
            url,
            project,
            report,
            started: Instant::now(),
            requests: VecDeque::new(),
            events: VecDeque::new(),
            request_count: 0,
        }
    }

    fn absorb(&mut self, event: DashEvent) {
        match event {
            DashEvent::Request {
                method,
                path,
                elapsed,
            } => {
                self.request_count += 1;
                self.requests.push_front(RequestRow {
                    method,
                    path,
                    elapsed,
                });
                self.requests.truncate(HISTORY);
            }
            DashEvent::Reloaded { millis } => self.push_event(
                EventKind::Ok,
                format!("reloaded in {}", format_millis(millis)),
            ),
            DashEvent::BuildFailed { message } => {
                // Only the first line: a compiler error can be a page long, and
                // the browser overlay is where the full text belongs.
                let first = message.lines().next().unwrap_or("build failed").to_string();
                self.push_event(EventKind::Fail, first);
            }
            DashEvent::Note { message } => self.push_event(EventKind::Note, message),
        }
    }

    fn push_event(&mut self, kind: EventKind, message: String) {
        self.events.push_front(EventRow {
            at: Instant::now(),
            kind,
            message,
        });
        self.events.truncate(HISTORY);
    }

    /// Draw until the user quits or the channel closes.
    ///
    /// The server runs on its own runtime; this owns the main thread and does
    /// nothing but poll, absorb, and redraw. `poll` carries the frame budget —
    /// there is no timer thread and no redraw when nothing changed beyond the
    /// clock.
    pub fn run(mut self, guard: &mut TerminalGuard, rx: Receiver<DashEvent>) -> io::Result<()> {
        loop {
            while let Ok(event) = rx.try_recv() {
                self.absorb(event);
            }

            guard.terminal().draw(|frame| self.render(frame))?;

            if event::poll(Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    if self.handle_key(key) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// `true` when the key means "quit".
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            // Ctrl+C has to be handled explicitly: raw mode means the terminal
            // no longer turns it into SIGINT for us.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::Char('c') => {
                self.requests.clear();
                self.events.clear();
                false
            }
            _ => false,
        }
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        // The wordmark is 6 rows of block art plus breathing room — a quarter of
        // a 30-row terminal, which measurably starved the component list. It
        // earns that space only on a tall window; everywhere else it collapses
        // to a one-line mark so the panels it introduces stay useful.
        let masthead_height: u16 = if area.height >= 34 { 8 } else { 3 };

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(masthead_height),
                Constraint::Length(1), // status
                Constraint::Length(3), // latency pulse
                Constraint::Min(8),    // tiers + requests
                Constraint::Length(5), // events
                Constraint::Length(1), // keys
            ])
            .split(area);

        if masthead_height > 3 {
            self.render_wordmark(frame, rows[0]);
        } else {
            self.render_compact_mark(frame, rows[0]);
        }
        self.render_status(frame, rows[1]);
        self.render_pulse(frame, rows[2]);

        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(42), Constraint::Min(30)])
            .split(rows[3]);
        self.render_tiers(frame, split[0]);
        self.render_requests(frame, split[1]);

        self.render_events(frame, rows[4]);
        self.render_keys(frame, rows[5]);
    }

    /// A panel frame: rounded, deep-gold, title carried in the border, and a
    /// column of padding so nothing touches the edge.
    ///
    /// Rounded corners are not decoration for its own sake — square corners read
    /// as a box drawn *around* content, rounded ones as a surface the content
    /// sits on, and the whole screen is calmer for it.
    fn panel(title: &str) -> Block<'_> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .padding(Padding::horizontal(1))
            .title(Span::styled(format!(" {title} "), theme::accent()))
    }

    /// The full masthead: the ANSI-shadow wordmark lit by a vertical gradient,
    /// with the product line set beside it.
    ///
    /// Same art and same ramp as the boot banner, so starting the server and
    /// watching it are visibly one product. The glow runs cream at the crown to
    /// deep gold in the drop shadow — light catching the top edge of the
    /// letters, which is the whole idea behind the name.
    fn render_wordmark(&self, frame: &mut Frame, area: Rect) {
        const ART: [&str; 6] = [
            " █████╗ ██╗     ██████╗ ██████╗  ██████╗ ",
            "██╔══██╗██║     ██╔══██╗██╔══██╗██╔═══██╗",
            "███████║██║     ██████╔╝██║  ██║██║   ██║",
            "██╔══██║██║     ██╔══██╗██║  ██║██║   ██║",
            "██║  ██║███████╗██████╔╝██████╔╝╚██████╔╝",
            "╚═╝  ╚═╝╚══════╝╚═════╝ ╚═════╝  ╚═════╝ ",
        ];
        // Brightest at the crown, cooling downward — the reverse of the brand
        // ramp's own order, which runs deep to bright.
        const GLOW: [usize; 6] = [5, 4, 3, 2, 1, 0];

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(45), Constraint::Min(20)])
            .split(area);

        let mut art_lines: Vec<Line> = vec![Line::from("")];
        for (row, line) in ART.iter().enumerate() {
            art_lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default()
                    .fg(theme::BRAND_RAMP[GLOW[row]])
                    .add_modifier(Modifier::BOLD),
            )));
        }
        frame.render_widget(Paragraph::new(art_lines), columns[0]);

        // The literal mark sits beside the art because the ANSI-shadow font has
        // no apostrophe glyph — the block letters spell ALBDO, and the name has
        // to appear somewhere unambiguous.
        let mut side: Vec<Line> = vec![Line::from(""), Line::from("")];
        side.push(Line::from(brand_spans("ALB'DO")));
        side.push(Line::from(Span::styled(VERSION_LABEL, theme::value())));
        side.push(Line::from(""));
        side.push(Line::from(ladder_spans()));
        frame.render_widget(Paragraph::new(side), columns[1]);
    }

    /// The one-line fallback for short terminals.
    fn render_compact_mark(&self, frame: &mut Frame, area: Rect) {
        let mut spans = brand_spans("ALB'DO");
        spans.push(Span::raw("   "));
        spans.push(Span::styled(VERSION_LABEL, theme::value()));
        spans.push(Span::raw("   "));
        spans.extend(ladder_spans());
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(theme::border()),
            ),
            area,
        );
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled("  ● ", Style::default().fg(theme::OK)),
            Span::styled(self.mode.label(), theme::accent()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled(&self.url, theme::value()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled("up ", theme::label()),
            Span::styled(format_uptime(self.started.elapsed()), theme::value()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled(self.request_count.to_string(), theme::value()),
            Span::styled(" served", theme::label()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled(&self.project, theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// A sparkline of recent server-compute times — the shape of the harness,
    /// live.
    ///
    /// Oldest on the left, so the line reads the way time does. This is the one
    /// number ALBEDO is entitled to claim, and a run of flat bars along the
    /// floor is a more honest advertisement than any adjective.
    fn render_pulse(&self, frame: &mut Frame, area: Rect) {
        let block = Self::panel("server compute");
        if self.requests.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no requests yet — the pulse starts with the first one",
                    theme::dim(),
                )))
                .block(block),
                area,
            );
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let mut data: Vec<u64> = self
            .requests
            .iter()
            .take(width)
            .map(|row| pulse_height(row.elapsed))
            .collect();
        data.reverse();
        frame.render_widget(
            Sparkline::default()
                .data(&data)
                .style(Style::default().fg(theme::ACCENT))
                .block(block),
            area,
        );
    }

    /// The tier mix, then the components behind it.
    ///
    /// Bars are each tier's *share* of the component total, because the question
    /// a glance asks is "how much of this app is static", and a proportion
    /// answers it where a count does not.
    fn render_tiers(&self, frame: &mut Frame, area: Rect) {
        let block = Self::panel("tiers");

        let Some(report) = &self.report else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no build report yet",
                    theme::dim(),
                )))
                .block(block),
                area,
            );
            return;
        };

        let counts = [
            report.tier_a_count,
            report.tier_b_count,
            report.tier_c_count,
        ];
        let total: usize = counts.iter().sum();
        // 🔴 Item 4.9 T0, 2026-08-05. These three strings used to read
        // `zero JS · static` / `hydrated · {tier_b_hydration_bytes} kB` /
        // `streamed`, which was wrong twice over:
        //
        // 1. **The vocabulary was inverted.** Tier C — the only tier that ships
        //    component code — was labelled `streamed`, and Tier B, which ships
        //    none, was labelled `hydrated`.
        // 2. **`tier_b_hydration_bytes` is the fabricated number item 4.6
        //    deleted** — a sum of `WeightEstimator::estimate`
        //    (`500 + imports*100 + …`), a proxy for source size with no
        //    relationship to any shipped byte, billed against the tier that
        //    downloads nothing.
        //
        // 4.6 fixed `bin/albedo/printer.rs` and never swept this lane, so the
        // dashboard kept printing a number the build output had already
        // retracted. Same failure mode item 6.5's `BootReport` exists to
        // prevent: **three lanes describing one event three ways.** Kept
        // deliberately in step with `printer.rs` — if that file's wording
        // changes, change it here in the same edit.
        //
        // No byte figure here on purpose: the honest Tier-C number comes from
        // `compile_client_island_module` via `MeasuredTierBytes`, which the
        // dashboard's `TierReport` does not carry.
        let hints = [
            "zero JS · static".to_string(),
            "server-rendered · zero JS".to_string(),
            "client island".to_string(),
        ];

        let mut lines: Vec<Line> = vec![Line::from("")];
        for (index, label) in ["A", "B", "C"].iter().enumerate() {
            let width = 10usize;
            let filled = if total == 0 {
                0
            } else {
                ((counts[index] * width) as f64 / total as f64).round() as usize
            };
            let (lit, track) = theme::bar(filled, width);
            lines.push(Line::from(vec![
                Span::styled(*label, theme::tier(index)),
                Span::raw("  "),
                Span::styled(lit, theme::tier(index)),
                Span::styled(track, theme::dim()),
                Span::raw("  "),
                Span::styled(format!("{:>2}", counts[index]), theme::value()),
                Span::raw("  "),
                Span::styled(hints[index].clone(), theme::label()),
            ]));
        }
        lines.push(Line::from(""));

        let mut components = report.components.clone();
        components.sort_by(|left, right| {
            tier_rank(left.tier)
                .cmp(&tier_rank(right.tier))
                .then_with(|| left.name.cmp(&right.name))
        });
        let room = area.height.saturating_sub(7) as usize;
        for component in components.iter().take(room) {
            let rank = tier_rank(component.tier) as usize;
            lines.push(Line::from(vec![
                Span::styled("▍", theme::tier(rank)),
                Span::raw(" "),
                Span::styled(tier_label(component.tier), theme::tier(rank)),
                Span::raw("  "),
                Span::styled(component.name.clone(), theme::label()),
            ]));
        }

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// Newest request on top, as a real table so the columns align regardless of
    /// path length.
    ///
    /// The elapsed column is coloured by **speed, not category**: the palette
    /// already says brightness means live, and here it means fast. A screen of
    /// pale-cream numbers is a screen of sub-millisecond responses, legible
    /// without reading a single digit.
    fn render_requests(&self, frame: &mut Frame, area: Rect) {
        let block = Self::panel("requests");

        if self.requests.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "waiting for the first request…",
                    theme::dim(),
                )))
                .block(block),
                area,
            );
            return;
        }

        let room = area.height.saturating_sub(2) as usize;
        let rows: Vec<Row> = self
            .requests
            .iter()
            .take(room)
            .map(|row| {
                Row::new(vec![
                    Cell::from(Span::styled(row.method.clone(), theme::accent())),
                    Cell::from(Span::styled(row.path.clone(), theme::label())),
                    Cell::from(
                        Span::styled(format_elapsed(row.elapsed), latency_style(row.elapsed))
                            .into_right_aligned_line(),
                    ),
                ])
            })
            .collect();

        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(6),
                    Constraint::Min(12),
                    Constraint::Length(10),
                ],
            )
            .block(block),
            area,
        );
    }

    fn render_events(&self, frame: &mut Frame, area: Rect) {
        let block = Self::panel("events");

        let room = area.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = if self.events.is_empty() {
            vec![Line::from(Span::styled(
                "save a file to trigger a rebuild",
                theme::dim(),
            ))]
        } else {
            self.events
                .iter()
                .take(room)
                .map(|row| {
                    let (glyph, style) = match row.kind {
                        EventKind::Ok => ("✓", Style::default().fg(theme::OK)),
                        EventKind::Fail => ("✗", Style::default().fg(theme::ERR)),
                        EventKind::Note => ("·", theme::label()),
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("{:>4}s", row.at.elapsed().as_secs()),
                            theme::dim(),
                        ),
                        Span::raw("  "),
                        Span::styled(glyph, style),
                        Span::raw("  "),
                        Span::styled(row.message.clone(), theme::label()),
                    ])
                })
                .collect()
        };

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_keys(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled("  q", theme::accent()),
            Span::styled(" quit", theme::label()),
            Span::styled("   c", theme::accent()),
            Span::styled(" clear", theme::label()),
            Span::styled("      ALBEDO_NO_TUI=1", theme::dim()),
            Span::styled(" for the plain log", theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Left), area);
    }
}

/// The wordmark, lit letter-by-letter through the brand ramp.
fn brand_spans(text: &str) -> Vec<Span<'static>> {
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(theme::BRAND_RAMP[index.min(theme::BRAND_RAMP.len() - 1)])
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// The solar pricing ladder, dimmed so it reads as a footer to the mark rather
/// than competing with it.
fn ladder_spans() -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, name) in TIER_LADDER.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(*name, theme::label()));
    }
    spans
}

/// Height of one bar in the pulse, on a **log scale**.
///
/// Server-compute times here span nanoseconds to milliseconds — six orders of
/// magnitude. Plotted linearly, a single 3 ms outlier flattens every
/// sub-microsecond response to nothing and the chart shows one spike over an
/// empty floor, which is worse than no chart. A decade of latency becomes a
/// fixed step, so the shape stays legible across the whole range.
///
/// Anchored at 1 ns = 0 so the bars sit on a real floor rather than on the
/// smallest sample in the window, which would make an idle server look busy.
fn pulse_height(elapsed: Duration) -> u64 {
    let ns = elapsed.as_nanos().max(1) as f64;
    (ns.log10() * 100.0).round().max(0.0) as u64
}

/// Colour an elapsed span by speed. Sub-microsecond burns brightest; anything
/// past ten milliseconds cools into the warning hue, because at that point the
/// number is the story.
fn latency_style(elapsed: Duration) -> Style {
    let ns = elapsed.as_nanos();
    let color = if ns < 1_000 {
        theme::BRAND_RAMP[5]
    } else if ns < 1_000_000 {
        theme::ACCENT_SOFT
    } else if ns < 10_000_000 {
        theme::ACCENT
    } else {
        theme::ERR
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::A => 0,
        Tier::B => 1,
        Tier::C => 2,
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::A => "A",
        Tier::B => "B",
        Tier::C => "C",
    }
}

/// Smallest ALBEDO-scale unit — mirrors the server's `timing::format_elapsed`
/// so a number does not change shape depending on which surface prints it.
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

fn format_millis(millis: f64) -> String {
    if millis < 1.0 {
        format!("{:.0} µs", millis * 1000.0)
    } else {
        format!("{millis:.0} ms")
    }
}

fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::types::ComponentTierSummary;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn report() -> TierReport {
        let component = |name: &str, tier: Tier| ComponentTierSummary {
            name: name.to_string(),
            file: format!("src/routes/{name}.tsx"),
            tier,
            reason: "reason".to_string(),
            weight_bytes: 0,
        };
        TierReport {
            components: vec![
                component("Guestbook", Tier::B),
                component("Overview", Tier::A),
                component("Island", Tier::C),
            ],
            tier_a_count: 1,
            tier_b_count: 1,
            tier_c_count: 1,
            tier_b_hydration_bytes: 5836,
        }
    }

    fn dashboard() -> Dashboard {
        Dashboard::new(
            Mode::Dev,
            "http://127.0.0.1:3000".to_string(),
            "C:/app".to_string(),
            Some(report()),
        )
    }

    /// Render into an off-screen buffer and flatten it to text. Lets the layout
    /// be asserted without a terminal, which is the only way this is testable in
    /// CI at all.
    fn screen(dash: &Dashboard, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| dash.render(frame)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    /// The brand statement. These four names are the STRATEGY pricing ladder and
    /// the version is a product label — both are load-bearing text, not
    /// decoration, so they are pinned.
    #[test]
    fn the_masthead_carries_the_version_and_the_full_pricing_ladder() {
        let text = screen(&dashboard(), 120, 30);
        assert!(text.contains("ALB'DO"), "{text}");
        assert!(text.contains("V : BETA 1"), "{text}");
        for tier in TIER_LADDER {
            assert!(text.contains(tier), "missing ladder rung {tier}: {text}");
        }
    }

    #[test]
    fn the_tier_panel_shows_the_mix_and_the_components() {
        let text = screen(&dashboard(), 120, 30);
        assert!(text.contains("zero JS"), "{text}");
        assert!(text.contains("Guestbook"), "{text}");
        assert!(text.contains("Overview"), "{text}");

        // 🪤 Item 4.9 T0. This test used to assert `text.contains("5.7 kB")`
        // under the label "tier B payload" — it was **pinning the defect**.
        // That figure is `tier_b_hydration_bytes`, the `WeightEstimator` proxy
        // item 4.6 deleted from `albedo build`, billed against the tier that
        // downloads nothing. The dashboard lane was never swept, so the number
        // survived here with a test holding it in place.
        //
        // The assertion is inverted deliberately: **no kB figure may be
        // attributed to Tier B**, because there is no Tier-B payload to
        // measure. The report fixture still carries a non-zero
        // `tier_b_hydration_bytes`, so this fails the moment anyone prints it
        // again.
        assert!(
            !text.contains("5.7 kB"),
            "Tier B ships no component JS — no byte figure may be billed to it: {text}"
        );
        assert!(
            text.contains("server-rendered"),
            "Tier B's hint must say what it actually is: {text}"
        );
        assert!(
            text.contains("client island"),
            "Tier C is the tier that ships code, and must say so: {text}"
        );
    }

    #[test]
    fn a_request_reaches_the_requests_panel() {
        let mut dash = dashboard();
        dash.absorb(DashEvent::Request {
            method: "GET".to_string(),
            path: "/guestbook".to_string(),
            elapsed: Duration::from_micros(3100),
        });
        let text = screen(&dash, 120, 30);
        assert!(text.contains("GET"), "{text}");
        assert!(text.contains("/guestbook"), "{text}");
        assert!(text.contains("3.10 ms"), "{text}");
    }

    #[test]
    fn a_failed_build_shows_only_its_first_line() {
        let mut dash = dashboard();
        dash.absorb(DashEvent::BuildFailed {
            message: "expected `;`\n  --> src/routes/index.tsx:4\n   |\n".to_string(),
        });
        let text = screen(&dash, 120, 30);
        assert!(text.contains("expected `;`"), "{text}");
        assert!(
            !text.contains("index.tsx:4"),
            "the overlay owns the full text; the log keeps one line: {text}"
        );
    }

    /// Empty states are the first thing a new user sees, so they say what to do
    /// rather than showing an empty box.
    #[test]
    fn empty_panels_tell_the_user_what_to_expect() {
        let text = screen(&dashboard(), 120, 30);
        assert!(text.contains("waiting for the first request"), "{text}");
        assert!(text.contains("save a file"), "{text}");
    }

    #[test]
    fn a_dashboard_without_a_report_still_renders() {
        let dash = Dashboard::new(Mode::Serve, "http://x".into(), "p".into(), None);
        let text = screen(&dash, 100, 24);
        assert!(text.contains("no build report yet"), "{text}");
    }

    /// Raw mode means the terminal no longer turns ctrl+c into SIGINT — if this
    /// regresses, the only way out of the dashboard is killing the process.
    #[test]
    fn quit_keys_quit_and_clear_only_clears() {
        let mut dash = dashboard();
        assert!(dash.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(dash.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(dash.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));

        dash.absorb(DashEvent::Note {
            message: "hello".to_string(),
        });
        assert!(!dash.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(dash.events.is_empty(), "plain `c` clears the panels");
    }

    /// A tall terminal earns the block wordmark; a short one must not pay for
    /// it. Both branches are pinned because the fallback is what keeps the
    /// dashboard usable on a normal 30-row window.
    #[test]
    fn the_wordmark_appears_only_when_there_is_room_for_it() {
        let tall = screen(&dashboard(), 104, 38);
        assert!(tall.contains("█████╗"), "block art on a tall terminal");
        assert!(tall.contains("ALB'DO"), "the literal mark rides beside the art");
        assert!(tall.contains("V : BETA 1"), "{tall}");

        let short = screen(&dashboard(), 104, 30);
        assert!(
            !short.contains("█████╗"),
            "the art must collapse rather than starve the panels"
        );
        assert!(short.contains("ALB'DO"), "the mark itself always shows");
        assert!(short.contains("V : BETA 1"), "and so does the version");
    }

    /// Latencies here span six orders of magnitude; a linear pulse would show
    /// one spike over an empty floor.
    #[test]
    fn the_pulse_is_log_scaled_so_a_decade_is_a_fixed_step() {
        let micro = pulse_height(Duration::from_micros(1));
        let milli = pulse_height(Duration::from_millis(1));
        let ten_milli = pulse_height(Duration::from_millis(10));
        assert_eq!(milli - micro, 300, "three decades, three steps");
        assert_eq!(ten_milli - milli, 100, "one decade, one step");
        assert_eq!(pulse_height(Duration::from_nanos(1)), 0, "anchored at a real floor");
    }

    #[test]
    fn elapsed_uses_the_smallest_albedo_scale_unit() {
        assert_eq!(format_elapsed(Duration::from_nanos(840)), "840 ns");
        assert_eq!(format_elapsed(Duration::from_nanos(142_700)), "142.7 µs");
        assert_eq!(format_elapsed(Duration::from_micros(3_100)), "3.10 ms");
    }
}
