---
name: project_alkmy_brand_identity
description: ALKMY brand identity brief — clean-slate liquid-chrome aesthetic + alchemy naming spine; deliverable PDF at repo root
metadata: 
  node_type: memory
  type: project
  originSessionId: 550ccc05-b821-45a8-8393-90cb5e93e709
---

ALKMY visual/brand identity brief, authored 2026-07-08/09 as "senior design rep" from ~24 user-supplied reference images (`C:\Users\bisha\Downloads\inspo`). Deliverable: **`ALKMY_Brand_Identity_Brief.pdf`** at repo root (13pp). Source reproducible in **`brief/`** (`brief.html` + `build.py` + `assets/`); built HTML+CSS → headless Chrome `--print-to-pdf` (no weasyprint/playwright installed; Chrome is at `C:\Program Files\Google\Chrome\Application\chrome.exe`). Fonts: Archivo / Space Grotesk / Space Mono via Google Fonts.

**Aesthetic (CLEAN SLATE — user's explicit choice; NOT an extension of [[project_cli_restyle]]'s Halation champagne/gold):** liquid chrome / mercury + clear glass, floating single objects in true-black voids, white rim-light bloom, chromatic-aberration as the *only* colour accent, Swiss-brutalist grotesque + monospace lab-note marginalia, spec-sheet/catalogue-plate layouts, transmuted-anatomy motif (heart/brain/ribcage/hands/face in chrome — "the mortal made incorruptible"). Rule: **90% monochrome / 10% stage-colour / aberration = salt.**

**Naming spine = alchemical magnum opus. Four stages = four products (locked):**
- **Nigredo → FORGE** (backend, the black crucible/dissolution). ⚠️ User correction: **Nigredo IS Forge** — do NOT treat nigredo as a separate non-product "ground."
- **Albedo → ALB'DO** (frontend, silver mirror)
- **Citrinitas → CTRNI'TAS** (AI, solar gold #E8B33D)
- **Rubedo → RUB'DO** (fintech, oxblood #C1272D)
- **PHOSPHOR** = the light-bearer / client runtime (the bloom itself; the one non-stage apparatus). **ALKMY** = umbrella/the alchemist.
- Accents: ember (Forge, heat *inside* the black — still open whether to keep or go pure-black), phosphor-green #B8FFE3, aberration cyan #4DE1FF / magenta #FF3DCB.

**The apostrophe (ALB'DO, RUB'DO, CTRNI'TAS) is an intentional, permanent wordmark signature** = alchemical "removal of the superfluous"/distillation; treat as the hero glyph. Myth layer (approved for the doc): **Phaethon/Icarus — "the view from above," not the fall.** Audience of the brief: an external designer meeting ALBDO cold.

Gotcha that bit us: the editable `brief.html` first used `IMG::` placeholder tokens → images looked "missing" in the Launch preview. Fixed by switching to a real `brief/assets/` folder with relative `src` paths (build.py downscales all inspo images into it; one messy filename → `nkbs_can.jpg`). PDF always embedded images fine.
