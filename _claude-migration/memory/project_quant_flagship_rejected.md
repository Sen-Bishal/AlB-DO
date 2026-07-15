---
name: project_quant_flagship_rejected
description: "Quant app evaluated as Gate-4 flagship port and rejected — pure-client SPA, web-worker blocker, no server surface"
metadata: 
  node_type: memory
  type: project
  originSessionId: d13d3386-fd05-4377-a577-3d73a21408b0
---

`A:\quant-analysis\quant` (ALBEDO's own investor Monte-Carlo simulator, Next 16 / React 19 / Tailwind 4) was evaluated 2026-06-28 as the Gate-4 / Workstream-E flagship port and **rejected** — user's call: "lets skip this project."

**Why:** It exercises a narrow slice of Gate-4's target surface and hits one hard blocker.
- ~4,800 lines, **100% client SPA**: one giant `"use client"` `page.tsx` (1,978 lines, ~40 hooks), 1,577-line imperative `<canvas>` charts, 928-line pure-TS MC engine.
- **No server surface at all** — no file routes (1 page), no nested layouts, no error/loading boundaries, no `action()`, no async data, no zod. So ALBEDO's tiering / server-authoritative moat can't shine; the whole page is just the A3 structural-hydration fallback.
- **Hard blocker:** runs the sim in a **Web Worker** (`new Worker(new URL('./mc_engine.worker.ts', import.meta.url))`). ALBEDO has **zero browser-worker bundling** — confirmed via graphify: "Worker" in the codebase = the server-side QuickJS engine-pool thread (`crates/albedo-server/src/engine_pool.rs`), not browser workers. No worker-chunk emission, no `import.meta.url` worker resolution.

**How to apply:** Don't re-pick this app as the flagship. Its only virtues were narrative ("our own investor model runs on our runtime") and being a brutal hydration stress test — revisit ONLY as a secondary dogfood/demo piece IF web-worker bundling ever lands. The real Gate-4 flagship still needs the full server surface (routes + layouts + boundaries + `action()`+zod + async data). See [[project_gate1_d_hardening]] context and TODO.md Gate 4 "E".
