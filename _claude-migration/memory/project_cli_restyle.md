---
name: project_cli_restyle
description: "VISUALS TRACK (not the dev roadmap): the albedo CLI restyled from cold-cyan prototype to the warm champagne 'Halation' identity (A+B blend) — palette, hero, de-jargoned copy, aligned columns, Lumen tier bars, glow timing, quiet dev reload."
metadata: 
  node_type: memory
  type: project
  category: visuals
  originSessionId: 058ac778-3b3c-49b5-9cee-22621aa1d03d
---

> **Track:** Visuals / brand — design-and-aesthetic work (CLI look, wordmark, palette, product voice).
> Kept deliberately OUT of the feature/gate development roadmap; it ships on its own cadence and neither
> gates nor is gated by engineering milestones.

**The `albedo` CLI got a product-grade restyle (2026-07-02, uncommitted).** User's brief: "make it look like an
actual product, not a prototype… it should capture people the moment they type `albedo init`." Diagnosis:
the CLI was built on **cold cyan** (`ACCENT=81`, `BRAND_PALETTE=[45,51,87,123,159]`) — which fights the
brand. ALBEDO *is* light (the fraction a surface reflects); the flagship Halation is "the glow around
bright things," champagne gold on ink. The CLI looked like a generic dev tool. Plus internal jargon leaked
into user-facing help ("Phase K dev server", "boot a real AlbedoServer", "Tier-A inline, Tier-B opcodes")
and `init` had no hero moment.

**Direction chosen: "A + B blend"** (I mocked up 3 via show_widget: A Halation / B Lumen / C Press). A =
warm editorial glow (base aesthetic everywhere); B = "instrument for light" (luminance tier bars +
glow-timing) promoted to first-class where tiers are actually reported.

## What changed (all in `src/bin/albedo.rs` + `src/bin/albedo/printer.rs`)
- **Palette → champagne gold.** `ACCENT=179` (gold), `ACCENT_SOFT=223` (cream), `ACCENT_DEEP=137` (deep
  gold), `MUTED=245` (warm gray); `BRAND_PALETTE=[137,179,221,222,223,230]` (wordmark shimmer, deep→cream
  ascent). Everything flows through the `print_*` helpers, so changing these ~5 constants recolored the
  whole tool. Zero cyan remains.
- **Banner halo.** `print_banner` adds a dim champagne hairline `──────` under the "albedo" wordmark (the
  glow around a bright thing).
- **`init` hero rewritten** (`print_init_success`): wordmark + a poetic line ("a starter, lit — three
  components, one at each tier of light"), a `▸ next` block, and a close ("watch albedo sort them by how
  much they move").
- **De-jargoned copy.** Help command list + `serve` help rewritten to product voice — `dev` = "start the
  dev server — live reload", `serve` = "build and run the production server", etc. Tier vocabulary kept
  ONLY where it's a user-facing feature (the tier report), never in blurbs. (Internal `// Phase …` comments
  left as-is.)
- **Fixed real alignment bugs.** `print_command`/`print_option`/the tier rows padded the *styled* string,
  so ANSI escape bytes counted toward column width and skewed everything. Now they pad the PLAIN text then
  colorize; shared `COL_WIDTH=20`. Descriptions line up cleanly.
- **Lumen tier report** (`printer.rs`, restyled + gold constants): per-tier **luminance bars** via
  `TIER_LUMEN=[137,179,222]` (A settled = deep gold, C live island = brightest) sized to each tier's share
  of components; dropped the redundant duplicate tier letter. Shown ONLY in `albedo build`/`ship` via a new
  `show_tiers` flag threaded through `run_prod_build_with_budget` (NOT in serve/dev — a dev hot reload would
  reprint the whole breakdown every save).
- **Glow timing** (`colorize_timing_ms`): speed reads as brightness — ≤1ms cream (230), ≤50ms bright gold,
  ≤500ms gold, ≤2000ms deep gold, else warm red (167). Thresholds retuned so a normal ~570ms BUILD glows
  deep gold, not alarm-red (the old thresholds were tuned for sub-ms renders and made builds look broken).
- **Quiet dev reload.** `run_prod_build_with_budget` gained a `quiet` flag (guards all presentation but
  keeps warnings/errors); `run_prod_build_quiet` wraps it; `dev_watch_and_reload` uses it — so a save prints
  a single `✓ reloaded in ~270ms` line instead of the full build log. (Extracted the spinner closure into
  `build_work` so quiet mode skips the spinner too.)

## Verified (debug binary, temp scaffolds)
`albedo help` (aligned, gold, de-jargoned), `albedo init` (hero lands), `albedo build` (tier report +
luminance bars, `built in` in deep gold not red), `albedo dev` (startup verbose, reload = single clean
line, no tier spam). 26 bin tests green; clean build, no warnings. `NO_COLOR` path preserved (all helpers
early-return plain text). `.claude/launch.json` has a `halation-dev` config (`albedo dev --port 3009`).

## Boot masthead — `print_boot_banner` (2026-07-03, uncommitted)
The **server-boot intro** was upgraded from the compact `print_banner` line to a full block wordmark, per
user brief "make the intro captivating — that's what people see first." First attempt was a **hand-drawn**
█-block "ALB'DO" → misshapen, user rejected it. Fix: rendered **"ALBDO" in the FIGlet `ansi_shadow` font via
pyfiglet** (offline; `pip install pyfiglet`), embedded the exact 6-row × 41-col art as a verified `const ART`
in `print_boot_banner` (NO runtime figlet dep — the string is fixed; regenerate with `pyfiglet -f ansi_shadow
ALBDO` if the mark changes). Letters glow top-down `ROW_LUMEN=[230,223,222,221,179,137]` (cream crown →
deep-gold drop-shadow). Tagline: `gradient_text("ALB'DO")` + `Version Beta` (soft gold) + the solar tier
ladder `SOL / EQUINOX / UMBRA / PERSEPHONE` (canonical free→enterprise ascent; MUTED names, dim slashes —
user swapped out the original `· fast JSX for Rust · v{version}` tail 2026-07-03; names = STRATEGY's tiers), then a
41-wide champagne hairline. The apostrophe has no glyph
in ansi_shadow → big mark reads ALBDO, literal `ALB'DO` lives in the tagline. Wired ONLY at the two boot
sites (`run_serve_command`, dev's `run_dev_mode`); help screens keep compact `print_banner`. User picked the
font from a 4-option AskUserQuestion (ansi_shadow / slant / standard / big). Live-verified on Halation serve
(debug + installed release); `NO_COLOR` still shapes the block. Fresh release reinstalled to `.cargo\bin`.
See [[project_request_timings]] (the two boot-line features shipped together this session).

## Not done / follow-ups
- Double `▸ build` section on `albedo build` (project/server header from `run_dev_mode` + the build
  section) is pre-existing minor redundancy — left alone.
- Spinner's final `\r\x1b[2K` shows as literal `[2K` when stderr is PIPED (non-tty) — a capture artifact,
  clears fine in a real terminal.
- Related: [[project_dev_serve_unification]] (the new `albedo dev` this styles), [[project_halation_flagship]]
  (the brand identity source — champagne/gold, "glow around bright things").
