//! `albedo demo` — the instrument panel, as it is meant to look.
//!
//! ## What this is, stated plainly
//!
//! **This dashboard is a rendering of intent, not a report.** Every number on
//! screen is synthesised here in this file; nothing is measured, nothing is
//! sampled, and no part of it is wired to a running server. It exists to show
//! the *shape* of the instrument — the vocabulary, the layout, the way a tier
//! mix and a latency pulse and an authorization matrix sit next to each other —
//! ahead of the subsystems that will fill it.
//!
//! That is also why the verb is `demo` and not `dev`. `albedo dev` draws
//! [`super::dev`], where the governing rule is the opposite one: *if a number
//! appears, something measured it.* Keeping the two behind different verbs is
//! what lets both statements stay true. Nothing in this module may ever be
//! reached from `dev` or `serve`, and nothing in `dev` may read from here.
//!
//! ## Why it carries its own palette
//!
//! [`super::theme`] is "Halation" — champagne gold on ink, shared byte-for-byte
//! with `albedo/printer.rs` so a colour cannot drift between the two surfaces.
//! This module deliberately does **not** import it. The demo is a cold monochrome
//! void with blue for the few things meant to catch the eye, and mixing that into
//! the shared palette would break the one guarantee `theme` is there to make.

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Sparkline, Table,
};
use ratatui::Frame;

use super::dev::{TIER_LADDER, VERSION_LABEL};
use super::TerminalGuard;

// ─── palette ────────────────────────────────────────────────────────────────
//
// Monochrome ground, blue accent. The ramp runs deep so the *mass* of the
// screen reads as cold and nearly unlit — borders, bar tracks and fills all sit
// in the 17–25 band, which is where "very dark blue" actually lives.
//
// `ICE` and `GLACIER` break that rule on purpose and are rationed accordingly.
// A highlight that is genuinely dark on a black ground is not a highlight; it is
// invisible. So the brightest blues are spent on perhaps six elements per frame
// — the live glyphs, the hero deltas, the lit half of a bar — and everything
// else stays in the dark end. Retune the whole screen from these ten constants.

/// Near-black. Unlit bar track — present, not readable.
const INK: Color = Color::Indexed(234);
/// Very dark navy. Panel fills and the resting state of anything inactive.
const VOID_BLUE: Color = Color::Indexed(17);
/// Deep navy. Dividers and the quieter half of a two-tone bar.
const DEEP_BLUE: Color = Color::Indexed(18);
/// Steel. Panel borders — visible as structure, never as content.
const STEEL: Color = Color::Indexed(24);
/// Glacier. Secondary accent: sub-labels, the second rank of a ramp.
const GLACIER: Color = Color::Indexed(31);
/// Ice. The single brightest note. Rationed — live glyphs and hero values only.
const ICE: Color = Color::Indexed(39);

/// Paper white. The mark, and any value that is the point of its panel.
const WHITE: Color = Color::Indexed(255);
/// Bone. Ordinary values.
const BONE: Color = Color::Indexed(252);
/// Ash. Labels.
const ASH: Color = Color::Indexed(245);
/// Shadow. Anything deliberately receding.
const SHADOW: Color = Color::Indexed(240);

/// Vertical ramp for the wordmark: white at the crown, cooling into navy at the
/// drop shadow. The name is a light term; the mark should look lit from above.
const MARK_RAMP: [Color; 6] = [VOID_BLUE, STEEL, GLACIER, ASH, BONE, WHITE];

fn mark(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}
fn hero() -> Style {
    Style::default().fg(ICE).add_modifier(Modifier::BOLD)
}
fn value() -> Style {
    Style::default().fg(BONE)
}
fn strong() -> Style {
    Style::default().fg(WHITE).add_modifier(Modifier::BOLD)
}
fn label() -> Style {
    Style::default().fg(ASH)
}
fn dim() -> Style {
    Style::default().fg(SHADOW)
}
fn border() -> Style {
    Style::default().fg(STEEL)
}

/// How many request rows the demo keeps. Bounded for the same reason the live
/// dashboard bounds its own: an unbounded deque in a long-running process is a
/// leak that only shows up once nobody is watching.
const HISTORY: usize = 240;

/// Frame budget. Slow enough to read, fast enough that the pulse moves.
const TICK: Duration = Duration::from_millis(110);

// ─── synthetic signal ───────────────────────────────────────────────────────

/// xorshift64. Deliberately not a dependency: the demo needs *shaped* noise, not
/// good noise, and a fixed seed means the screen looks the same every time it is
/// opened — which is what makes a screenshot reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    /// Inclusive range.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high.saturating_sub(low).saturating_add(1))
    }
}

/// The routes the demo serves, each with the tier its route component resolved
/// to. Chosen to read as a real application with a real live surface: a few
/// pages, a dynamic one, and the internal lanes that make the thing interesting.
///
/// The tier travels with the request on purpose — it is what ties the log back
/// to the tier panel above it, so the two halves of the screen are visibly about
/// the same application rather than two unrelated readouts. The internal lanes
/// carry `·`: they are not components and have no tier to report.
const ROUTES: [(&str, &str, &str); 9] = [
    ("GET", "/", "A"),
    ("GET", "/atlas", "A"),
    ("GET", "/atlas/[id]", "B"),
    ("GET", "/vault", "B"),
    ("GET", "/vault/ledger", "B"),
    ("GET", "/signal", "C"),
    ("POST", "/_albedo/action", "·"),
    ("GET", "/_albedo/patches", "·"),
    ("GET", "/_albedo/phosphor", "·"),
];

struct RequestRow {
    method: &'static str,
    path: &'static str,
    tier: &'static str,
    micros: u64,
}

/// A named subsystem and its one-word state. This strip is the most important
/// thing on the screen: it is the only place the whole apparatus is visible at
/// once, and the names are doing more work than the numbers.
struct Subsystem {
    name: &'static str,
    state: &'static str,
    reading: String,
}

pub struct Demo {
    rng: Rng,
    started: Instant,
    frame: u64,
    requests: Vec<RequestRow>,
    served: u64,
    /// Self-tuning pass counter and the deltas it has accumulated.
    pass: u64,
    wire_delta: i64,
    ttfb_delta: i64,
    tier_flips: u64,
    rules: u64,
    next_pass: u64,
}

impl Default for Demo {
    fn default() -> Self {
        Self::new()
    }
}

impl Demo {
    pub fn new() -> Self {
        let mut demo = Self {
            rng: Rng(0x5EED_A1BE_D0_u64 | 1),
            started: Instant::now(),
            frame: 0,
            requests: Vec::new(),
            served: 48_211,
            pass: 4,
            wire_delta: -34,
            ttfb_delta: -41,
            tier_flips: 7,
            rules: 3,
            next_pass: 134,
        };
        // Seed the panels full before the first draw. A dashboard that spends
        // its first ten seconds saying "waiting for the first request" is
        // useless to photograph, and being photographed is this screen's
        // entire job.
        for _ in 0..HISTORY.min(64) {
            demo.push_request();
        }
        demo
    }

    /// Latencies are drawn from three bands so the pulse has texture: a floor of
    /// cached static reads, a body of ordinary server renders, and an occasional
    /// slow one. A chart with no variance reads as a placeholder.
    fn push_request(&mut self) {
        let (method, path, tier) = ROUTES[self.rng.below(ROUTES.len() as u64) as usize];
        let micros = match self.rng.below(100) {
            0..=54 => self.rng.between(38, 140),
            55..=92 => self.rng.between(140, 720),
            93..=98 => self.rng.between(720, 2_400),
            _ => self.rng.between(2_400, 9_000),
        };
        self.requests.insert(
            0,
            RequestRow {
                method,
                path,
                tier,
                micros,
            },
        );
        self.requests.truncate(HISTORY);
        self.served += 1;
    }

    fn tick(&mut self) {
        self.frame += 1;

        // One or two requests per frame keeps the pulse moving at a rate that
        // looks like traffic rather than like a benchmark loop.
        let burst = self.rng.between(1, 2);
        for _ in 0..burst {
            self.push_request();
        }

        if self.next_pass > 0 {
            self.next_pass -= 1;
        } else {
            // A self-tuning pass lands: the counters step, the deltas deepen a
            // little, and the clock resets. Deltas are clamped so the demo never
            // drifts into a claim that reads as absurd.
            self.pass += 1;
            self.rules += self.rng.between(1, 2);
            self.tier_flips += self.rng.between(1, 3);
            self.wire_delta = (self.wire_delta - self.rng.between(0, 2) as i64).max(-46);
            self.ttfb_delta = (self.ttfb_delta - self.rng.between(0, 2) as i64).max(-53);
            self.next_pass = 210;
        }
    }

    fn subsystems(&self) -> [Subsystem; 6] {
        [
            Subsystem {
                name: "FORGE",
                state: "sealed",
                reading: format!("{} w/s", group(1_180 + self.frame % 190)),
            },
            Subsystem {
                name: "PHOSPHOR",
                state: "carrying",
                reading: format!("{} lanes", 300 + self.frame % 26),
            },
            Subsystem {
                name: "PRISM",
                state: "resolved",
                reading: format!("{} topics", 312 + self.frame % 9),
            },
            Subsystem {
                name: "APERTURE",
                state: "stopped down",
                reading: "0 egress".to_string(),
            },
            Subsystem {
                name: "SHUTTER",
                state: "open",
                reading: "99.9% pass".to_string(),
            },
            Subsystem {
                name: "CTRNI'TAS",
                state: "tuning",
                reading: format!("pass {:03}", self.pass),
            },
        ]
    }

    /// Draw until the user quits.
    pub fn run(mut self, guard: &mut TerminalGuard) -> io::Result<()> {
        loop {
            guard.terminal().draw(|frame| self.render(frame))?;

            if event::poll(TICK)? {
                if let Event::Key(key) = event::read()? {
                    if quits(key) {
                        return Ok(());
                    }
                }
            }
            self.tick();
        }
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        // Same threshold as the live dashboard: the block art is six rows plus
        // breathing room, and below a tall window it starves the panels it is
        // introducing.
        let masthead: u16 = if area.height >= 36 { 8 } else { 3 };

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(masthead),
                Constraint::Length(1), // status
                Constraint::Length(4), // subsystem strip
                Constraint::Min(11),   // tiers · pulse · self-tuning
                Constraint::Length(11), // requests · forge · reach
                Constraint::Length(1), // keys
            ])
            .split(area);

        if masthead > 3 {
            self.render_wordmark(frame, rows[0]);
        } else {
            self.render_compact_mark(frame, rows[0]);
        }
        self.render_status(frame, rows[1]);
        self.render_subsystems(frame, rows[2]);

        let band_a = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                // 38 rather than 30: at 30 the inner width leaves nine columns
                // for the tier hint and "server-rendered" arrives as
                // "server-re". A truncated label is worse than no label.
                Constraint::Length(38),
                Constraint::Min(24),
                Constraint::Length(34),
            ])
            .split(rows[3]);
        self.render_tiers(frame, band_a[0]);
        self.render_pulse(frame, band_a[1]);
        self.render_tuning(frame, band_a[2]);

        let band_b = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(30),
                Constraint::Length(30),
                Constraint::Length(34),
            ])
            .split(rows[4]);
        self.render_requests(frame, band_b[0]);
        self.render_forge(frame, band_b[1]);
        self.render_reach(frame, band_b[2]);

        self.render_keys(frame, rows[5]);
    }

    /// Rounded, steel-bordered, one column of padding. Rounded corners read as a
    /// surface the content rests on; square ones read as a box drawn around it.
    fn panel(title: &str) -> Block<'_> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border())
            .padding(Padding::horizontal(1))
            .title(Span::styled(format!(" {title} "), Style::default().fg(GLACIER)))
    }

    fn render_wordmark(&self, frame: &mut Frame, area: Rect) {
        const ART: [&str; 6] = [
            " █████╗ ██╗     ██████╗ ██████╗  ██████╗ ",
            "██╔══██╗██║     ██╔══██╗██╔══██╗██╔═══██╗",
            "███████║██║     ██████╔╝██║  ██║██║   ██║",
            "██╔══██║██║     ██╔══██╗██║  ██║██║   ██║",
            "██║  ██║███████╗██████╔╝██████╔╝╚██████╔╝",
            "╚═╝  ╚═╝╚══════╝╚═════╝ ╚═════╝  ╚═════╝ ",
        ];
        // Brightest at the crown, cooling down into the shadow.
        const GLOW: [usize; 6] = [5, 4, 3, 2, 1, 0];

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(45), Constraint::Min(20)])
            .split(area);

        let mut art: Vec<Line> = vec![Line::from("")];
        for (row, line) in ART.iter().enumerate() {
            art.push(Line::from(Span::styled(
                format!("  {line}"),
                mark(MARK_RAMP[GLOW[row]]),
            )));
        }
        frame.render_widget(Paragraph::new(art), columns[0]);

        // The ANSI-shadow font has no apostrophe glyph, so the block letters
        // spell ALBDO and the real mark has to appear somewhere unambiguous.
        let side = vec![
            Line::from(""),
            Line::from(""),
            Line::from(mark_spans("ALB'DO")),
            Line::from(vec![
                Span::styled(VERSION_LABEL, Style::default().fg(GLACIER)),
                Span::styled("   demo", dim()),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "the compiler emits the backend",
                label(),
            )),
            Line::from(ladder_spans()),
        ];
        frame.render_widget(Paragraph::new(side), columns[1]);
    }

    fn render_compact_mark(&self, frame: &mut Frame, area: Rect) {
        let mut spans = mark_spans("ALB'DO");
        spans.push(Span::raw("   "));
        spans.push(Span::styled(VERSION_LABEL, Style::default().fg(GLACIER)));
        spans.push(Span::raw("   "));
        spans.extend(ladder_spans());
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(border()),
            ),
            area,
        );
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled("  ● ", Style::default().fg(ICE)),
            Span::styled("live", strong()),
            Span::styled("  ·  ", dim()),
            Span::styled("127.0.0.1:3000", value()),
            Span::styled("  ·  ", dim()),
            Span::styled("up ", label()),
            Span::styled(format_uptime(self.started.elapsed()), value()),
            Span::styled("  ·  ", dim()),
            Span::styled(group(self.served), value()),
            Span::styled(" served", label()),
            Span::styled("  ·  ", dim()),
            Span::styled("build ", label()),
            Span::styled("a7f3e1c", value()),
            Span::styled("  ·  ", dim()),
            Span::styled("cold start 0", label()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The apparatus, in one row. Every name here is a real subsystem with a
    /// real place in the architecture; the readings beside them are not.
    fn render_subsystems(&self, frame: &mut Frame, area: Rect) {
        let systems = self.subsystems();
        let slots = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 6); 6])
            .split(area);

        for (index, system) in systems.iter().enumerate() {
            let lines = vec![
                Line::from(vec![
                    Span::styled("● ", Style::default().fg(ICE)),
                    Span::styled(system.name, strong()),
                ]),
                Line::from(Span::styled(system.state, Style::default().fg(GLACIER))),
                Line::from(Span::styled(system.reading.clone(), dim())),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(DEEP_BLUE))
                        .padding(Padding::horizontal(1)),
                ),
                slots[index],
            );
        }
    }

    fn render_tiers(&self, frame: &mut Frame, area: Rect) {
        const COUNTS: [u64; 3] = [18, 7, 3];
        const HINTS: [&str; 3] = ["zero JS · static", "server-rendered", "client island"];
        const COMPONENTS: [(&str, &str); 7] = [
            ("A", "Atlas"),
            ("A", "Colophon"),
            ("A", "Masthead"),
            ("B", "Ledger"),
            ("B", "Signal"),
            ("C", "Dial"),
            ("C", "Scrubber"),
        ];

        let total: u64 = COUNTS.iter().sum();
        let mut lines: Vec<Line> = vec![Line::from("")];

        for (index, letter) in ["A", "B", "C"].iter().enumerate() {
            let width = 8usize;
            let filled = ((COUNTS[index] * width as u64) as f64 / total as f64).round() as usize;
            let filled = filled.min(width);
            lines.push(Line::from(vec![
                Span::styled(*letter, Style::default().fg(tier_color(index)).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled("█".repeat(filled), Style::default().fg(tier_color(index))),
                Span::styled("░".repeat(width - filled), Style::default().fg(INK)),
                Span::raw("  "),
                Span::styled(format!("{:>2}", COUNTS[index]), value()),
                Span::raw("  "),
                Span::styled(HINTS[index], label()),
            ]));
        }
        lines.push(Line::from(""));

        let room = area.height.saturating_sub(7) as usize;
        for (letter, name) in COMPONENTS.iter().take(room) {
            let rank = match *letter {
                "A" => 0,
                "B" => 1,
                _ => 2,
            };
            lines.push(Line::from(vec![
                Span::styled("▍ ", Style::default().fg(tier_color(rank))),
                Span::styled(*letter, Style::default().fg(tier_color(rank))),
                Span::raw("  "),
                Span::styled(*name, label()),
            ]));
        }

        frame.render_widget(Paragraph::new(lines).block(Self::panel("tiers")), area);
    }

    /// Log-scaled, for the same reason the live dashboard is: these times span
    /// microseconds to milliseconds, and plotted linearly one slow response
    /// flattens every fast one into the floor.
    fn render_pulse(&self, frame: &mut Frame, area: Rect) {
        let inner_w = area.width.saturating_sub(4) as usize;
        let inner_h = area.height.saturating_sub(4);

        let mut data: Vec<u64> = self
            .requests
            .iter()
            .take(inner_w)
            .map(|row| pulse_height(row.micros))
            .collect();
        data.reverse();

        let sorted = {
            let mut all: Vec<u64> = self.requests.iter().map(|row| row.micros).collect();
            all.sort_unstable();
            all
        };
        let pick = |q: f64| -> u64 {
            if sorted.is_empty() {
                return 0;
            }
            let index = ((sorted.len() as f64 - 1.0) * q).round() as usize;
            sorted[index]
        };

        let block = Self::panel("server compute");
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(block.inner(area));
        frame.render_widget(block, area);

        if inner_h > 0 {
            frame.render_widget(
                Sparkline::default()
                    .data(&data)
                    .style(Style::default().fg(GLACIER)),
                split[0],
            );
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("p50 ", label()),
                Span::styled(format_micros(pick(0.50)), hero()),
                Span::styled("   p95 ", label()),
                Span::styled(format_micros(pick(0.95)), value()),
                Span::styled("   p99 ", label()),
                Span::styled(format_micros(pick(0.99)), value()),
                Span::styled("   max ", label()),
                Span::styled(format_micros(pick(1.0)), dim()),
            ])),
            split[1],
        );
    }

    /// The self-tuning panel. Two build generations, and the distance between
    /// them — the only claim on this screen that is about the compiler rather
    /// than the application.
    fn render_tuning(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("pass ", label()),
                Span::styled(format!("{:03}", self.pass), strong()),
                Span::styled("   converged", Style::default().fg(GLACIER)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("wire     ", label()),
                Span::styled("▁▂▃▅▆█", Style::default().fg(DEEP_BLUE)),
                Span::styled(format!("  {:>3}%", self.wire_delta), hero()),
            ]),
            Line::from(vec![
                Span::styled("ttfb     ", label()),
                Span::styled("▁▂▄▅▇█", Style::default().fg(DEEP_BLUE)),
                Span::styled(format!("  {:>3}%", self.ttfb_delta), hero()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("tier flips        ", label()),
                Span::styled(format!("{:>5}", self.tier_flips), value()),
            ]),
            Line::from(vec![
                Span::styled("rules applied     ", label()),
                Span::styled(format!("{:>5}", self.rules), value()),
            ]),
            Line::from(vec![
                Span::styled("next pass in      ", label()),
                Span::styled(format!("{:>5}", format_countdown(self.next_pass)), value()),
            ]),
            Line::from(""),
            // What the tuning is derived from. Without it the deltas above read
            // as assertions; with it they read as the output of something.
            Line::from(vec![
                Span::styled("basis         ", label()),
                Span::styled(format!("{:>5}", group(41_800 + self.served % 400)), value()),
                Span::styled(" traces", label()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(Self::panel("self-tuning")),
            area,
        );
    }

    fn render_requests(&self, frame: &mut Frame, area: Rect) {
        let room = area.height.saturating_sub(2) as usize;
        let rows: Vec<Row> = self
            .requests
            .iter()
            .take(room)
            .map(|row| {
                let tier_style = match row.tier {
                    "A" => Style::default().fg(tier_color(0)),
                    "B" => Style::default().fg(tier_color(1)),
                    "C" => Style::default().fg(tier_color(2)),
                    _ => dim(),
                };
                // Tier sits beside the method, not out by the latency: the path
                // column is the elastic one, so anything after it drifts away
                // from what it describes as the panel widens.
                Row::new(vec![
                    Cell::from(Span::styled(row.method, Style::default().fg(GLACIER))),
                    Cell::from(Span::styled(row.tier, tier_style)),
                    Cell::from(Span::styled(row.path, label())),
                    Cell::from(
                        Span::styled(format_micros(row.micros), latency_style(row.micros))
                            .into_right_aligned_line(),
                    ),
                ])
            })
            .collect();

        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(5),
                    Constraint::Length(2),
                    Constraint::Min(12),
                    Constraint::Length(10),
                ],
            )
            .block(Self::panel("requests")),
            area,
        );
    }

    fn render_forge(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("crucible      ", label()),
                Span::styled("sealed", strong()),
            ]),
            Line::from(vec![
                Span::styled("writes/s      ", label()),
                Span::styled(format!("{:>6}", group(1_180 + self.frame % 190)), value()),
            ]),
            Line::from(vec![
                Span::styled("partitions    ", label()),
                Span::styled(format!("{:>6}", 312), value()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("atomicity     ", label()),
                Span::styled("✓ 72 kills", Style::default().fg(ICE)),
            ]),
            Line::from(vec![
                Span::styled("oversold      ", label()),
                Span::styled(format!("{:>6}", 0), strong()),
            ]),
            Line::from(vec![
                Span::styled("torn writes   ", label()),
                Span::styled(format!("{:>6}", 0), strong()),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines).block(Self::panel("forge")), area);
    }

    /// The authorization matrix, compressed to the one question it answers:
    /// what can each read be reached by. A read keyed by principal is reachable
    /// only by that principal; one keyed by a route parameter is reachable by
    /// anyone who can name it.
    fn render_reach(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("principals        ", label()),
                Span::styled(format!("{:>5}", 312), value()),
            ]),
            Line::from(vec![
                Span::styled("keyed topics      ", label()),
                Span::styled(format!("{:>5}", 312), value()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("by principal  ", label()),
                Span::styled("████████", Style::default().fg(ICE)),
                Span::styled(" 100%", strong()),
            ]),
            Line::from(vec![
                Span::styled("by parameter  ", label()),
                Span::styled("░░░░░░░░", Style::default().fg(INK)),
                Span::styled("    0", dim()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("unkeyed reads     ", label()),
                Span::styled(format!("{:>5}", 0), strong()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(Self::panel("reach")),
            area,
        );
    }

    fn render_keys(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled("  q", Style::default().fg(GLACIER)),
            Span::styled(" quit", label()),
            Span::styled("        albedo demo", dim()),
            Span::styled(" — a rendering of intent; every value here is synthetic", dim()),
        ]);
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Left), area);
    }
}

/// `true` when the key means quit. Ctrl+C has to be caught explicitly: raw mode
/// means the terminal no longer turns it into SIGINT.
fn quits(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (matches!(key.code, KeyCode::Char('c'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn mark_spans(text: &str) -> Vec<Span<'static>> {
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            Span::styled(
                ch.to_string(),
                mark(MARK_RAMP[(MARK_RAMP.len() - 1).saturating_sub(index.min(3))]),
            )
        })
        .collect()
}

fn ladder_spans() -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, name) in TIER_LADDER.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(VOID_BLUE)));
        }
        spans.push(Span::styled(*name, dim()));
    }
    spans
}

fn tier_color(index: usize) -> Color {
    // A settled and deep, C live and bright — luminance, not hue, so the mix of
    // a build is legible before any label is read.
    [DEEP_BLUE, GLACIER, ICE][index.min(2)]
}

/// Where the pulse's floor sits, as a log10 of microseconds. ≈30 µs.
const PULSE_FLOOR_LOG: f64 = 1.48;

/// Log-scaled bar height, **rebased** so the floor is the fastest response the
/// panel expects rather than zero.
///
/// The rebase is the whole trick. Plotted against a zero origin this band —
/// roughly 40 µs to 9 ms, only 2.4 decades — puts every bar between 40% and
/// 100% of full height, and the panel renders as a solid mass of block with a
/// ragged top edge: technically a chart, visually a rectangle. Subtracting the
/// floor spends the entire panel height on the range that actually varies.
///
/// Still log, not linear, for the reason the live dashboard is: one slow
/// response must not flatten every fast one into the baseline.
fn pulse_height(micros: u64) -> u64 {
    let log = (micros.max(1) as f64).log10();
    ((log - PULSE_FLOOR_LOG).max(0.0) * 220.0).round() as u64
}

fn latency_style(micros: u64) -> Style {
    let color = if micros < 150 {
        ICE
    } else if micros < 1_000 {
        BONE
    } else if micros < 5_000 {
        ASH
    } else {
        SHADOW
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn format_micros(micros: u64) -> String {
    if micros < 1_000 {
        format!("{micros} µs")
    } else {
        format!("{:.2} ms", micros as f64 / 1_000.0)
    }
}

fn format_countdown(frames: u64) -> String {
    let seconds = frames / 9;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Thousands separator. A six-digit count with no grouping reads as noise.
fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
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

/// Entry point for the `demo` verb.
///
/// Unlike [`super::available`], this checks only for a terminal. `NO_COLOR` and
/// `RUST_LOG` opt out of the *live* dashboard because there the plain log is a
/// real alternative; here there is nothing to fall back to, and the demo has no
/// logs to be scribbled over by.
pub fn run() -> Result<(), String> {
    if !io::stdout().is_terminal() {
        return Err(
            "albedo demo draws to the alternate screen, so it needs a terminal — \
             it cannot be piped or redirected."
                .to_string(),
        );
    }

    let mut guard =
        TerminalGuard::new().map_err(|err| format!("failed to claim the terminal: {err}"))?;
    Demo::new()
        .run(&mut guard)
        .map_err(|err| format!("demo dashboard error: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(demo: &Demo, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| demo.render(frame)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    /// The point of the screen. These six names are the apparatus, and they are
    /// the reason the frame is worth photographing at all.
    #[test]
    fn the_subsystem_strip_names_every_apparatus() {
        let text = screen(&Demo::new(), 150, 42);
        for name in [
            "FORGE",
            "PHOSPHOR",
            "PRISM",
            "APERTURE",
            "SHUTTER",
            "CTRNI'TAS",
        ] {
            assert!(text.contains(name), "missing subsystem {name}: {text}");
        }
    }

    #[test]
    fn the_masthead_carries_the_mark_version_and_ladder() {
        let text = screen(&Demo::new(), 150, 42);
        assert!(text.contains("ALB'DO"), "{text}");
        assert!(text.contains(VERSION_LABEL), "{text}");
        for rung in TIER_LADDER {
            assert!(text.contains(rung), "missing rung {rung}: {text}");
        }
    }

    /// The disclosure is not decoration. A screen full of invented numbers has
    /// to say so somewhere on the screen itself, because a screenshot travels
    /// without the command that produced it.
    #[test]
    fn the_footer_discloses_that_the_values_are_synthetic() {
        let text = screen(&Demo::new(), 150, 42);
        assert!(text.contains("synthetic"), "{text}");
    }

    /// Seeded full at construction — the first frame is the photograph.
    #[test]
    fn the_first_frame_is_already_populated() {
        let demo = Demo::new();
        assert!(demo.requests.len() >= 32);
        let text = screen(&demo, 150, 42);
        assert!(text.contains("p50"), "{text}");
        assert!(text.contains("µs") || text.contains("ms"), "{text}");
    }

    /// Both masthead branches have to survive, same as the live dashboard's.
    #[test]
    fn it_draws_at_both_masthead_heights() {
        let demo = Demo::new();
        let tall = screen(&demo, 150, 42);
        let short = screen(&demo, 150, 30);
        assert!(tall.contains("█"), "expected block art in the tall branch");
        assert!(short.contains("ALB'DO"), "{short}");
    }

    /// A fixed seed is what makes the screenshot reproducible.
    #[test]
    fn the_seeded_signal_is_deterministic() {
        assert_eq!(screen(&Demo::new(), 150, 42), screen(&Demo::new(), 150, 42));
    }

    #[test]
    fn quit_keys_are_recognised() {
        assert!(quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    }

    /// 🪤 The pulse rendered as a **solid rectangle** before the rebase landed.
    ///
    /// Against a zero origin the demo's own latency band occupies only the top
    /// 40–100% of the scale, so every bar clears 40% of the panel and the chart
    /// becomes a filled block with a ragged top. The rebase is what buys the
    /// shape back, and a plain "it is log-scaled" assertion does not catch its
    /// removal — this pins the *ratio*, which is the thing that was broken.
    #[test]
    fn the_pulse_rebase_keeps_the_fast_band_off_the_ceiling() {
        let fastest = pulse_height(38);
        let slowest = pulse_height(9_000);
        assert!(fastest < slowest / 8, "fastest {fastest}, slowest {slowest}");

        // Still logarithmic: a decade is a fixed step regardless of where it
        // sits, so one slow response cannot flatten every fast one.
        let decade_low = pulse_height(1_000) - pulse_height(100);
        let decade_high = pulse_height(10_000) - pulse_height(1_000);
        assert_eq!(decade_low, decade_high);
    }

    #[test]
    fn counts_are_grouped_for_reading() {
        assert_eq!(group(48_211), "48 211");
        assert_eq!(group(312), "312");
        assert_eq!(group(1_284), "1 284");
    }
}
