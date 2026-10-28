# Brag Plan: AlB'DO

## What is this app?

A JSX/TSX render compiler, database and HTTP server written in Rust: you write
React-shaped components, and one binary compiles and serves them with no Node.js
in the request path. The compiler decides *per component* how much client
JavaScript it needs — often none — and a collection declared in config becomes a
table, a live subscription and a typed `.d.ts` with no server directory, no ORM
and no API route.

## The angle

Every React framework makes **you** declare the boundary. `"use client"` is a
confession: the tooling could not work out what your component costs, so it
asked you. AlB'DO's whole thesis is that the boundary is a *derivable fact*, not
an annotation — so the video opens on the directive nobody should have to write,
deletes it, and then shows the compiler doing the job itself.

This is specific to this project in a way no other video could be: the hook is
the exact line of React ceremony AlB'DO exists to abolish, and every number on
screen is one this repo measured on this machine.

## Hook (first 2-3 seconds)

A single mono line on true black, typed character by character:

```
"use client"
```

It holds. Then, in serif beneath it: **You shouldn't have to say this.**
The directive burns away, leaving the black.

That is the whole pitch in four words, and any React developer reads it
instantly.

## Key moments (the middle)

- **The wordmark.** `ALB'DO` full-bleed, the apostrophe carrying the chrome
  gradient on its own (it is the wordmark signature in `Hero.tsx`). Tagline
  beneath: *Evolution simplified.*
- **The compiler deciding.** The three-tier classification table arrives one row
  at a time — the actual mechanism, lifted verbatim from the README. Tier A ships
  plain HTML with zero JS; Tier B ships 250–400 bytes of bindings; Tier C
  hydrates. Eyebrow above it: *decided at build time · nothing to configure.*
- **The live write (centerpiece).** Two browser tabs side by side. A cursor types
  a guestbook message in the left tab and submits. The row lands in **both** tabs
  at the same instant. Caption: *No API route. No subscription code.*
- **The receipts.** Three measured numbers, arriving on the beat, with the
  honest qualifier on screen under them.

## Outro / punchline

The wordmark slams back full-screen with the tagline and the repo path. The last
thing on screen is the thing you can go read.

## User flow worth showing

Yes — and it is the centerpiece (Scene 4). From `scaffold/albedo.config.ts` and
`scaffold/src/routes/guestbook.tsx`:

1. **Entry** — a collection is declared as a block in `albedo.config.ts`
   (`guestbook: { fields: { author: "text", message: "text" } }`). That is the
   entire backend; the table, the query and the seed rows are emitted from it.
2. **Key action** — a visitor types a message into `/guestbook` and submits.
   The route calls `append("guestbook", …)` — a server action, no endpoint.
3. **Result** — the row lands in every open tab with no polling and no
   subscription code, and what crosses the wire is the row that changed, not the
   list it belongs to.

The guestbook file's own comment says it best: *"the whole point of ALB'DO in one
file."* That is the scene.

## Tone

- Preset: **cinematic**
- Creative direction: *a compiler trailer — the quiet confidence of something
  that already measured itself*
- Interpretation: Big type, full-bleed frames, dramatic reveals rather than quick
  cuts. Scenes enter at scale 0.95 and settle to 1.0. Short declarative lines,
  each landing before the next begins. The restraint is doing the work: on true
  black with one chrome accent, motion should feel weighted and deliberate, never
  busy. No exclamation marks, no hype adjectives — the numbers are the drama.

## Format: landscape — 1920x1080
## Duration: 21.6 seconds (5 scenes)

## Visual identity (from the project)

Extracted from `scaffold/src/styles.css` (`:root`) and `Hero.tsx`.

- Background: `#000000` (`--void`, true black — the stylesheet's comment says it
  is chosen so an OLED panel actually powers the pixels down). Raised surface
  `#0d0d11`, sunk `#08080b`.
- Text: `#f4f4f7` (`--ink`); soft `#b9b9c4`; muted `#9a9aa6`; faint `#7a7a86`.
- Accent: the **chrome gradient** — the one ornament in the whole design system:
  `linear-gradient(177deg, #6f6f7d 0%, #b4b4c2 13%, #f2f2f8 27%, #ffffff 37%,
  #8d8d9c 49%, #4a4a56 60%, #9d9dab 71%, #e6e6ee 84%, #5a5a67 95%, #7c7c8a 100%)`
  For small type use the quieter ramp: `#9a9aa8 → #d6d6e0 → #ffffff → #b0b0be →
  #8a8a98 → #c2c2ce`.
- Rules / hairlines: `rgba(255,255,255,0.14)`; soft `rgba(255,255,255,0.07)`.
- Display font: serif — `"Iowan Old Style", "Palatino Linotype", Palatino,
  Georgia, ui-serif, serif`. Use the stack verbatim so the render degrades the
  same way the app does (on this machine it lands on Palatino Linotype).
- Body / data font: mono — `ui-monospace, "SF Mono", "Cascadia Code",
  "Roboto Mono", Menlo, Consolas, monospace`.
- Strongest visual element: the `ALB'DO` wordmark with the apostrophe set as its
  own `.glyph` span carrying the chrome. **Critical rule from the stylesheet:** on
  a black ground the chrome's dark stops must keep a raised floor (nothing under
  `#4a4a56`) or the band punches a hole through the letterform. Keep the
  alternation steep — shallow stops read as a grey smear.

## Share copy (draft)

> Your framework makes you write `"use client"` because it never looked at your
> component. AlB'DO looks. One Rust binary: compiler, database and server —
> 70 µs to render and serve, no Node in the request path.

## Audio direction

- Role: **cinematic support** — a low, forward bed that carries weight between
  the reveals rather than driving them.
- Music: `happy-beats-business-moves-vol-11-by-ende-dot-app.mp3` (114.84 BPM).
  Chosen because it is the only bundled track with a strong cue at **1.60s** — the
  hook line needs a hit that early — and its strong cues then spread evenly
  (3.70 / 5.80 / 8.96 / 12.65 / 17.91) across all five scenes. Caveat noted
  honestly: the bundled library is an upbeat "Business Moves" set, so it is not a
  true cinematic bed. Treat it low and filtered so it reads as pulse and weight,
  not as corporate brightness.
- Music treatment: start at 0.0s, fade in over ~0.5s, sit low (≈0.30–0.38) under
  the whole piece, lift slightly into the receipts, then **duck under the final
  wordmark** and fade out across the last ~1.2s so the last frame is nearly
  silent. That silence is the cinematic move.
- Music cue guidance: bundled preset read at
  `assets/music/cues/happy-beats-business-moves-vol-11-by-ende-dot-app.music-cues.json`.
  Tempo 114.84 BPM, beat ≈0.523s.
  - **Strong-cue locks (3, the maximum the brief allows):** `1.60s` (hook line
    lands), `3.70s` (wordmark slam), `12.65s` (typing begins in the live-write
    scene).
  - **Beat-grid windows:** tier rows at `8.96 / 10.01 / 11.06` (every *other*
    beat, ≈1.05s apart, safely above the 0.8s reading floor); receipt numbers at
    `16.86 / 17.39 / 17.91` (fast arrival, full set then held 1.6s).
- Audio-reactive treatment: **subtle**. Drive only the chrome gradient's
  brightness/position on the wordmark and a faint vignette lift from music RMS.
  No waveform bars, no equalizers, no particles, no strobing. If extraction is
  unavailable, skip it — do not block the render.
- SFX posture: **sparse**, motion-matched. This is a restrained piece; a handful
  of precise sounds beat a dense layer.
- Audio-coupled moments: the `"use client"` typing (key ticks); the directive
  burning away (a short reverse/dissolve accent); the wordmark slam (one dry
  impact); each tier row arriving (soft interface tick); the guestbook typing and
  submit (keys, then a click); **the row landing in both tabs at once (the payoff
  — one clean confirm, and it should feel like the only sound in the room)**;
  the receipts arriving (three light ticks).
- Restraint rule: **no sound on the final wordmark except the music fading out.**
  Nothing whooshes. No riser into the outro, no stinger on the repo path. The
  piece ends by getting quieter, not louder. Total SFX count should stay in the
  single digits.

## Storyboard

### Scene 1 — "use client" — 3.70s  (0.00 → 3.70)

True black, full-bleed, nothing else on screen. Centered mono in `--ink-soft`,
at large scale: `"use client"` types in character by character from ~0.55s.
It settles and holds. At **1.60s** the serif line fades up beneath it:
**You shouldn't have to say this.** From ~2.9s the directive desaturates and
burns away upward, leaving only the serif line, which holds to the cut.

Reading budget: `"use client"` (2 tokens, ~0.8s floor) is settled 1.3s+;
the serif line is 6 words (~1.8s floor) and is settled from 1.60 → 3.70 = 2.10s. ✓

Sequential/interaction: yes — the directive types character by character; the
serif line is a single fade-up, not typed.
Audio intent: near-silence with a low bed entering; the typing is the loudest
thing. The viewer should lean in.
Audio-coupled idea: key ticks on the typed characters (sparse — not one per
character if it reads as a rattle); a short dissolve accent as the directive burns.
Music: low, filtered, barely present. `// beat-locked: 1.60s` — serif line lands.
Transition mood: dramatic → Scene 2

### Scene 2 — the wordmark — 4.21s  (3.70 → 7.91)

Hard, weighted reveal at **3.70s**: `ALB'DO` full-bleed in the serif display
stack, set in chrome, entering at scale 0.95 → 1.0 with the gradient sweeping
across the letterforms as it settles. The apostrophe is its own glyph and
catches the brightest band. A hairline rule (`rgba(255,255,255,0.14)`) draws in
beneath it. At 4.75s the tagline fades up in `--ink-soft` serif:
*Evolution simplified.*

Reading budget: wordmark (1 word, huge) settled 4.2s; tagline (2 words, ~0.8s
floor) settled 4.75 → 7.91 = 3.16s. ✓ Generous holds are the point here.

Sequential/interaction: none — this is a single dramatic reveal, held.
Audio intent: the first real weight in the piece. One impact, then the bed opens up.
Audio-coupled idea: a single dry impact on the wordmark landing. Nothing else.
Music: bed lifts. `// beat-locked: 3.70s` — wordmark slam.
Transition mood: dramatic wipe → Scene 3

### Scene 3 — the compiler decides — 4.21s  (7.91 → 12.12)

Eyebrow in small mono `--ink-faint`, present from 7.91s:
`DECIDED AT BUILD TIME · NOTHING TO CONFIGURE`. Below it, three mono rows arrive
**one at a time**, each sliding up a few pixels into place, separated by hairline
rules — the three-tier table verbatim from the README:

```
Tier A   no hooks, no async      →  plain HTML, zero JS
Tier B   event handlers          →  bindings only, ~250–400 B
Tier C   full hooks / async      →  full client hydration
```

The `→` and the right-hand result are set in the quiet chrome ramp; the left is
plain ink. All three hold together for the last ~1.06s.

Reading budget: rows land 8.96 / 10.01 / 11.06 — **1.05s apart, above the 0.8s
floor**, and the full set is settled 11.06 → 12.12. Deliberately every *other*
beat, not every beat: at 114 BPM a per-beat reveal (0.52s) would outrun reading.

Sequential/interaction: yes — 3 rows arriving one by one, then the set holds.
Audio intent: mechanical and precise. Each row is a small, dry event.
Audio-coupled idea: one soft interface tick per row arrival, fired at the same
timestamp as the motion.
Music: steady bed underneath. `// beat-grid: row 1 at 8.96s, row 2 at 10.01s, row 3 at 11.06s`
Transition mood: clean → Scene 4

### Scene 4 — the live write (CENTERPIECE) — 4.74s  (12.12 → 16.86)

Two browser tab frames side by side on the black ground, raised to `#0d0d11`
with hairline borders — both showing the same `/guestbook` route with the two
seed rows (`ada — first light`, `alan — the machine stirs`). Small mono labels:
`tab 1` / `tab 2`.

At **12.65s** a cursor moves into the left tab's message field and types
`it just arrives` (author field already reads `bishal`). At ~14.22s the cursor
clicks the submit button — a visible press state. At **14.76s** the new row
appears in **both tabs simultaneously**, highlighted briefly, while the existing
rows visibly do not move or re-render.

Caption fades up at 14.76s in mono beneath both tabs:
**No API route. No subscription code.**

Reading budget: caption is 6 words (~1.8s floor), settled 14.76 → 16.86 = 2.10s. ✓
The typed string is short and supported by the motion, not a read.

Sequential/interaction: yes — simulated cursor move, typing, a click with a press
state, then a simultaneous two-pane result. This is the product doing its thing.
Audio intent: the payoff of the whole video. Build small with the typing, then
**one clean confirm on the simultaneous landing, and let it sit in silence.**
Audio-coupled idea: key ticks while typing; a single click on submit; one
confirm/announcement cue exactly on the row landing in both panes. Nothing layered
over that moment.
Music: duck very slightly under the landing so the confirm reads.
`// beat-locked: 12.65s` — typing begins. `// beat-grid: submit 14.22s, row lands 14.76s`
Transition mood: dramatic → Scene 5

### Scene 5 — the receipts, then the name — 4.73s  (16.86 → 21.59)

**Part A (16.86 → 19.49).** Three measured numbers arrive fast and then hold as a
set, large chrome numerals with small mono labels beneath each:

```
70 µs          13.6 µs             434 ms
render + serve  action dispatch    clean build
```

At 17.91s a small `--ink-faint` mono line fades up under the row — and it stays
for the whole hold:

`measured on one machine · loopback, keep-alive · pre-1.0`

**Part B (19.49 → 21.59).** Cut to black, then the wordmark slams back
full-bleed in chrome — `ALB'DO` — with the serif tagline *Evolution simplified*
and, in small faint mono, `github.com/Sen-Bishal/AlB-DO`. Held 2.10s while the
music fades out to near silence.

Reading budget: numbers arrive 16.86 / 17.39 / 17.91 (fast — permitted because
the full set then holds) and the complete set plus qualifier is settled from
17.91 → 19.49 = 1.58s. Outro: wordmark + 2-word tagline + a repo path, settled
2.10s. ✓

Sequential/interaction: yes — 3 numbers arriving in quick succession, then the
set holds with its qualifier.
Audio intent: a controlled lift through the numbers, then **withdrawal**. The
final frame is the quietest in the video.
Audio-coupled idea: three light ticks on the numbers. **Nothing on the final
wordmark** — only the music fading.
Music: slight lift under Part A, then duck and fade out across the last ~1.2s.
`// beat-grid: 16.86s, 17.39s, 17.91s`
Transition mood: hard cut to black → outro hold → end

**Music mood for this video:** cinematic (bundled upbeat track, treated low and
filtered to read as weight rather than brightness)

**Audio summary:** Near-silence and typing, one dry impact on the wordmark, three
precise mechanical ticks as the compiler classifies, a single clean confirm when
the row lands in both tabs at once, three light ticks on the receipts — and then
the music withdraws so the name ends the piece in quiet.

---

## Honesty notes (carried into the composition)

These are constraints on the copy, not decoration:

1. **Every number on screen is from this repo's own README**, measured on this
   machine: 70 µs render+serve and 13.6 µs action dispatch are *loopback,
   keep-alive, uncontended* on a Ryzen 9 9900X; 434 ms is a clean `albedo build`
   of the 5-component scaffold. The qualifier line in Scene 5 is **not optional** —
   the README itself says there is no published head-to-head benchmark, and the
   video must not claim more than the repo does.
2. **Nothing from `albedo demo` is used.** Those figures are placeholder.
3. The tier byte range (~250–400 B) and the three-tier behaviour are quoted from
   the README's own classification table.

---

## As built — where the composition deviates from this plan

Recorded rather than quietly absorbed, because the plan above is the contract.

1. **Typeface substitution.** The plan specifies the app's own stack
   (`Iowan Old Style / Palatino Linotype / Palatino / Georgia`). Hyperframes
   pre-bundles a fixed set of 18 families and embeds them as data URIs; a name
   outside that set either trips a lint warning and relies on local system-font
   capture, or fails closed in a cloud render. Palatino/Iowan are not in the
   set and are not redistributable. The build therefore uses **EB Garamond**
   (the nearest bundled old-style humanist serif — same register as Palatino,
   not a Didone like the bundle's other serif) for display, and **IBM Plex
   Mono** for all data and UI type. Both embed deterministically, so the video
   renders identically anywhere. The colour system is unchanged and exact.

2. **The audio-reactive channel lifts instead of darkening.** The plan called
   for "a faint vignette lift" from music RMS. Implemented as written, it was a
   measurable no-op: the ground is `#000000`, so a darkening vignette composites
   black over black and changes nothing — confirmed by measuring corner
   luminance at a low-RMS and a high-RMS frame (both 0). It was rebuilt as a
   faint centre **lift** (`#glow`, a soft `rgba(214,214,224,…)` radial behind
   every scene) whose opacity is driven per frame by the same signal. Verified
   on the rendered file: the same background region reads Y=1.85 at the
   quietest outro frame and Y=3.20 at the loudest. Chrome brightness on the
   wordmark and numerals is driven by band 0 (bass), capped at +7%.

   Both channels are written as zero-duration `tl.set()` tweens rather than
   `tl.call()` callbacks, because the renderer samples by seeking and a `set`
   resolves correctly on an arbitrary seek where a callback may not.

3. **Structure is monolithic, not sub-composed.** `lint` emits seven warnings
   asking for one sub-composition per scene. They are Studio-timeline
   readability warnings, not correctness ones — `check` reports 0 errors and
   the render is unaffected. Left as a single file so the five scenes share one
   continuous absolute-time timeline, which is what keeps the beat locks
   readable. Worth revisiting if this composition gets edited in Studio.

## Verified output

- `brag.mp4` — 1920x1080, 30fps, **648 frames, 21.600s exactly**, H.264 +
  AAC stereo, 2.6 MB. Audio measured: mean -25.5 dB, peak -6.6 dB (no clipping;
  deliberately quiet — see the note below).
- Frame 0 is the baked poster (verified by extraction).
- `check`: 0 errors · Runtime 0 · Layout 0 · Motion 0 · **Contrast 56/56 WCAG AA**.

⚠️ **Loudness caveat.** The mix is quiet by design (a low bed under a restrained
piece). At mean -25.5 dB it will sound noticeably softer than typical social
video, which is mastered nearer -16 to -14 LUFS. If this is posted to a feed
rather than viewed full-screen, raise the bed and SFX gains before re-rendering.
