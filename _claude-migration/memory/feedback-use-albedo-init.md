---
name: feedback-use-albedo-init
description: "Always scaffold a new Albedo app with `albedo init <project>`, never hand-roll the project files."
metadata: 
  node_type: memory
  type: feedback
  originSessionId: 7a21bef9-399d-4f94-9ce7-4c2fcf92bca7
---

When creating a NEW Albedo project, use the real scaffolder: `albedo init <project>`
(usage: `albedo init <project> [--force]`, run from the parent dir). Do NOT
hand-create `albedo.config.ts` / `package.json` / `tsconfig.json` / `src/...` by
copying another app's files.

**Why:** The user built the `init` command specifically for this. Hand-rolling
skips scaffolding details and gets things subtly wrong — in the 2026-06-18
portfolio attempt I hand-made the project and the global stylesheet never applied
(a body `<link rel="stylesheet" href="/styles.css">` came back with `linkHref:
null` — stripped by the renderer), so the page rendered completely unstyled. The
binding-mode *features* all worked live (two fine-grained islands, useState/
useMemo/derived/className, zero round-trip) but the demo looked broken because of
the hand-rolled scaffold. The user was (rightly) disappointed.

**How to apply:** For "make a new albedo app / demo / project", FIRST run
`albedo init <name>`, then inspect what it scaffolds (especially how global CSS is
wired — that's the part hand-rolling gets wrong), then add components/content on
top of that structure. Only then `albedo build` + `albedo serve`. See
[[design_tier_classification]] for the binding-mode features the demo is meant to
showcase.

**DONE (2026-06-18):** the `A:\albedo-portfolio` demo was rebuilt properly via
`albedo init --force` + Bishal Sen content; works live in `albedo serve` :3113. See
[[project_dogfood_portfolio]].

**Root-cause CORRECTION:** the rule (use `albedo init`, never hand-roll) still
holds — it gets the dev pipeline + scaffold right. BUT the 2026-06-18 unstyled-page
was **not merely a hand-rolling artifact**: production CSS genuinely does not work
(`assets.css` is always `[]`; the inline-`<style>` concat is dev-only). Even a clean
`albedo init` app ships unstyled in `albedo serve` unless you drop `styles.css` in
`public/` + hand-write a `<link>`. Tracked as **A4 "userland boundary"** in TODO.md
(see [[project_dogfood_portfolio]]). So: `albedo init` is right practice, but it
does NOT by itself fix production styling today.
