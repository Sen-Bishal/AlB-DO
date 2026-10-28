# Hyperframes Composition Brief: AlB'DO

## Objective

Create a short launch-style brag video for AlB'DO — a JSX/TSX render compiler,
database and HTTP server written in Rust that ships as one binary with no Node.js
in the request path.

## Output

- Composition directory: `brag-output/composition/`
- Rendered video: `brag-output/brag.mp4`
- Format: landscape — 1920x1080
- Duration: 21.6 seconds

## Source Material

- Project root: `C:/Development/ALKMY/AlB-DO`
- Primary files read:
  - `README.md` (three-tier table, latency/build benchmark tables)
  - `scaffold/src/styles.css` (the entire visual identity — `:root` tokens)
  - `scaffold/src/components/Hero.tsx` (the wordmark and tagline)
  - `scaffold/src/routes/index.tsx` (the six product claims)
  - `scaffold/src/routes/guestbook.tsx` (the live-write flow)
  - `scaffold/albedo.config.ts` (the FORGE collection declaration)
- Product name: **AlB'DO** (rendered `ALB'DO` in the wordmark)
- Tagline / strongest claim: **Evolution simplified** /
  *"A compiler that reads your components and decides what each one actually
  costs."*
- Key UI or visual moment to recreate: **the `/guestbook` route in two browser
  tabs, where a submitted row lands in both at the same instant** — this is the
  centerpiece, not a landing-page recreation.
- Copy that must appear verbatim:
  - `"use client"`
  - `You shouldn't have to say this.`
  - `ALB'DO`
  - `Evolution simplified`
  - `DECIDED AT BUILD TIME · NOTHING TO CONFIGURE`
  - `Tier A   no hooks, no async      →  plain HTML, zero JS`
  - `Tier B   event handlers          →  bindings only, ~250–400 B`
  - `Tier C   full hooks / async      →  full client hydration`
  - `No API route. No subscription code.`
  - `70 µs` / `render + serve`
  - `13.6 µs` / `action dispatch`
  - `434 ms` / `clean build`
  - `measured on one machine · loopback, keep-alive · pre-1.0`
  - `github.com/Sen-Bishal/AlB-DO`

> **The qualifier line is mandatory.** The project's own README states there is no
> published head-to-head benchmark and that the numbers are measured on one
> machine. The video must not claim more than the repo does. Do not drop, shorten
> or shrink-to-illegible `measured on one machine · loopback, keep-alive · pre-1.0`.

## Creative Direction

- Tone preset: **cinematic**
- Creative direction: *a compiler trailer — the quiet confidence of something that
  already measured itself*
- Interpretation: Big type, full-bleed frames, dramatic reveals rather than quick
  cuts. Scenes enter at scale 0.95 and settle to 1.0. Short declarative lines that
  land one at a time. Weighted, deliberate motion — never busy. The restraint is
  the style: true black, one chrome accent, generous holds.
- Angle: Every React framework makes *you* declare the boundary. `"use client"` is
  a confession that the tooling could not work out what your component costs, so
  it asked you. AlB'DO's thesis is that the boundary is a derivable fact, not an
  annotation. So the video opens on the directive nobody should have to write,
  burns it away, and then shows the compiler doing that job itself — classifying
  components, then serving a live backend that was declared in six lines of config.
- Hook: `"use client"` types onto black; the serif line **You shouldn't have to
  say this.** lands beneath it at 1.60s; the directive burns away.
- Outro / punchline: the `ALB'DO` wordmark slams back full-bleed with *Evolution
  simplified* and the repo path, held in near-silence as the music fades out.
- Avoid:
  - Generic SaaS language ("streamline", "supercharge", "unlock", "elevate")
  - Abstract filler visuals — no gradient meshes, glowing blobs, particle fields
  - Unrelated visual redesign — the palette and type below are the product's own
  - Any number not in the verbatim list above
  - Exclamation marks and hype adjectives

## Visual Identity

All values lifted from `scaffold/src/styles.css` `:root`.

- Background: `#000000` (`--void`). Raised surface `#0d0d11`, sunk `#08080b`.
- Text: `#f4f4f7` (`--ink`), soft `#b9b9c4`, muted `#9a9aa6`, faint `#7a7a86`.
- Accent — **the chrome gradient**, the only ornament in the design system:
  ```css
  linear-gradient(177deg,
    #6f6f7d 0%, #b4b4c2 13%, #f2f2f8 27%, #ffffff 37%, #8d8d9c 49%,
    #4a4a56 60%, #9d9dab 71%, #e6e6ee 84%, #5a5a67 95%, #7c7c8a 100%)
  ```
  Quieter ramp for small type / numerals:
  ```css
  linear-gradient(177deg,
    #9a9aa8 0%, #d6d6e0 26%, #ffffff 42%, #b0b0be 58%, #8a8a98 76%, #c2c2ce 100%)
  ```
  Apply with `background-clip: text; -webkit-background-clip: text; color: transparent`.
- Rules / hairlines: `rgba(255,255,255,0.14)`, soft `rgba(255,255,255,0.07)`.
- Display font: `"Iowan Old Style", "Palatino Linotype", Palatino, Georgia,
  ui-serif, "Times New Roman", serif` — use the stack verbatim; it resolves to
  Palatino Linotype on this machine. Do not substitute a webfont.
- Body / data font: `ui-monospace, "SF Mono", "Cascadia Code", "Roboto Mono",
  Menlo, Consolas, monospace`.
- Visual references from the project:
  - The `ALB'DO` wordmark with the apostrophe as its own `.glyph` span carrying
    the chrome (see `Hero.tsx` — it is the wordmark signature).
  - **Chrome-on-black rule, straight from the stylesheet's own comment:** on a
    black ground any gradient stop near the background punches a *hole* through
    the letterform. Keep a raised floor — nothing under `#4a4a56` — and put the
    contrast in the bright half. Keep the alternation steep; shallow stops read as
    a grey smear.
  - `color-scheme: dark` — the document is dark by declaration.

## Storyboard

Use the storyboard in `brag-output/brag-plan.md` as the creative contract. It
carries per-scene reading budgets that have already been checked against the
0.8s-per-label / 0.3s-per-word floor — preserve those holds.

Scene summary:

1. **"use client"** — 3.70s (0.00 → 3.70) — the directive types on, the serif line
   **You shouldn't have to say this.** lands at 1.60s, the directive burns away.
2. **The wordmark** — 4.21s (3.70 → 7.91) — `ALB'DO` full-bleed in chrome at
   scale 0.95 → 1.0 with a gradient sweep, hairline rule, tagline *Evolution
   simplified* at 4.75s.
3. **The compiler decides** — 4.21s (7.91 → 12.12) — eyebrow
   `DECIDED AT BUILD TIME · NOTHING TO CONFIGURE`, then the three tier rows arrive
   one at a time at 8.96 / 10.01 / 11.06, holding as a set.
4. **The live write (CENTERPIECE)** — 4.74s (12.12 → 16.86) — two `/guestbook`
   tabs side by side with the seed rows; a cursor types `it just arrives` from
   12.65s, clicks submit at 14.22s, and the new row appears in **both tabs
   simultaneously** at 14.76s while untouched rows visibly do not move. Caption
   `No API route. No subscription code.` from 14.76s.
5. **The receipts, then the name** — 4.73s (16.86 → 21.59) — three numbers arrive
   16.86 / 17.39 / 17.91 and hold as a set with the mandatory qualifier line; hard
   cut at 19.49s to the wordmark outro with the repo path, held 2.10s.

## Audio

- Audio role: **cinematic support** — a low bed carrying weight between reveals,
  never driving them.
- Audio arc: near-silence and typing → one dry impact on the wordmark → three
  precise mechanical ticks as the compiler classifies → a single clean confirm on
  the simultaneous row landing (the payoff, sitting almost alone) → three light
  ticks on the receipts → the music withdraws so the name ends in quiet.
- Music: `happy-beats-business-moves-vol-11-by-ende-dot-app.mp3` (114.84 BPM),
  copied to `brag-output/composition/assets/music/`.
- Music treatment: start 0.0s, fade in ~0.5s, sit low (≈0.30–0.38) throughout,
  lift slightly under the receipts, then **duck under the final wordmark and fade
  out across the last ~1.2s**. The final frame should be close to silent — that
  withdrawal is the cinematic move, so do not let the track run out at full level.
  The bundled library is an upbeat "Business Moves" set rather than a true
  cinematic bed; keep it low and dark so it reads as pulse and weight, not
  corporate brightness.
- Music cue guidance: bundled preset at
  `<skill-dir>/assets/music/cues/happy-beats-business-moves-vol-11-by-ende-dot-app.music-cues.json`
  (tempo 114.84 BPM, beat ≈0.523s).
  - **Strong-cue locks (3):** `1.60s` serif hook line · `3.70s` wordmark slam ·
    `12.65s` typing begins. Mark each `// beat-locked`.
  - **Beat-grid:** tier rows `8.96 / 10.01 / 11.06` (every *other* beat — a
    per-beat reveal at 0.52s would outrun reading); receipt numbers
    `16.86 / 17.39 / 17.91`. Mark each `// beat-grid`.
- Audio-reactive treatment: **subtle**. Drive only (a) the chrome gradient's
  brightness or background-position on the wordmark and (b) a faint vignette lift,
  from music RMS. No waveform bars, no equalizers, no particles, no strobing, no
  heavy pulsing. If extraction is unavailable, document it and skip — do not block
  the render.
- Audio-coupled moments:
  - Scene 1, typing — sparse key ticks (not one per character if it rattles)
  - Scene 1, directive burns — one short dissolve/reverse accent
  - Scene 2, wordmark lands at 3.70s — one dry impact, nothing else
  - Scene 3, each tier row — one soft interface tick at the same timestamp as the motion
  - Scene 4, typing and submit — key ticks, then a single click
  - **Scene 4, row lands in both tabs at 14.76s — one clean confirm cue, unlayered.
    This is the payoff; let it sit.**
  - Scene 5, three numbers — three light ticks
  - Scene 5, final wordmark — **nothing**
- SFX selection guidance: sparse and motion-matched; total SFX count should stay
  in the single digits. Choose files after the animation exists. `interface` and
  `ui` families suit the tier rows and the submit click; `keyboard` suits the two
  typing moments; a short dry `impact` suits the wordmark. Nothing decorative,
  nothing that whooshes.
- SFX analysis guidance: use
  `<skill-dir>/assets/sfx/sfx-analysis.md` (and its JSON) for selection. Prefer
  low high-frequency-risk files — this piece is quiet and polished, and a harsh
  tick will be the only thing anyone hears.
- Exact SFX choice: Hyperframes chooses filenames, timestamps, density and volume
  based on the implemented animation.
- Audio files: copy the chosen music and any selected SFX into
  `brag-output/composition/assets/`.

## Hyperframes Instructions

Load the composition-building Hyperframes domain skills — `hyperframes-core`
(composition contract + `data-*` timing), `hyperframes-animation` (motion),
`hyperframes-creative` (design spec, beats, audio-reactive), `hyperframes-keyframes`
(seek-safe keyframes), and `hyperframes-cli` (lint/check/render). /brag is its own
workflow: do not enter the `hyperframes` entry-point intent interview and do not
route into its generic promo / launch-video workflow. Prefer native Hyperframes
conventions over anything in `/brag`.

Requirements:

- Show at least one real UI, copy, or visual element from the source project —
  here, the `/guestbook` two-tab live write in Scene 4 plus the wordmark.
- Keep all text readable in the final render; the per-scene reading budgets in
  `brag-plan.md` are already computed — honour the holds.
- Keep the video within 15-25 seconds (target 21.6s).
- Include the planned music and SFX layer.
- Treat `/brag` audio notes as guidance, not a fixed cue sheet. Choose SFX after
  the visual animation exists.
- Treat music cue metadata as optional timing hints; ignore any cue that hurts
  readability, scene pacing, or the product story.
- Major reveals may move toward nearby strong cues within ~0.15s; smaller
  entrances may align to nearby beats within ~0.10s. Use only the 3 strong-cue
  locks listed above.
- Honour the music fade-out under the final wordmark.
- Consider the audio-reactive workflow for the subtle chrome/vignette treatment
  described above.
- Use local assets for audio and any runtime dependencies.
- Run `hyperframes check` before render — it is brag's single gate.
