# Claude Code migration bundle

Everything here is LOCAL Claude Code state ferried from the old rig (`A:\AlBDO-v-0.1.0`)
to the new one (`C:\Development\ALKMY\AlB-DO`). Claude Code stores nothing in the cloud, so
these files ARE the memory and config.

## What's in here
- `memory/`  — the knowledge base Claude loads on startup (MEMORY.md + all topic files).
- `global/`  — user-level config (graphify skill, global CLAUDE.md, settings.json).
- (transcripts / `*.jsonl` are NOT here — move those on a USB if you want `/resume`
  history; they're ~80MB and would bloat git history permanently even after untracking.)

## The one thing that will trip you up: the project folder is keyed by absolute path
Claude buckets per-project state under a SANITIZED copy of the repo ROOT's absolute
path (every `:` `\` `.` becomes `-`; hyphens are preserved).
Old path `A:\AlBDO-v-0.1.0` -> `A--AlBDO-v-0-1-0`.

The repo (`AlB-DO.git`) is being cloned INTO `C:\Development\ALKMY`, so a default clone
makes the repo root `C:\Development\ALKMY\AlB-DO`, which sanitizes to:
  **`C--Development-ALKMY-AlB-DO`**
You must restore the memory into that folder or Claude starts amnesiac.

## Restore steps (on the new rig)
1. Clone: `cd C:\Development\ALKMY && git clone https://github.com/Sen-Bishal/AlB-DO.git`
2. Install Claude Code. `cd C:\Development\ALKMY\AlB-DO && claude` once, log in, then EXIT.
   -> This auto-creates the correctly-named bucket under `.claude\projects\`.
      Don't guess the name — use whichever folder just appeared. It should be
      `C--Development-ALKMY-AlB-DO`, but let the tool be the source of truth.
3. Global config:
   cp -r global/skills        "C:/Users/bisha/.claude/skills"
   cp    global/CLAUDE.md     "C:/Users/bisha/.claude/CLAUDE.md"
   cp    global/settings.json "C:/Users/bisha/.claude/settings.json"
4. Project memory (into the folder from step 2):
   cp -r memory/* "C:/Users/bisha/.claude/projects/C--Development-ALKMY-AlB-DO/memory/"
   (create the memory/ subfolder if it isn't there yet)
5. Re-launch `claude` in the repo — MEMORY.md loads on startup as before.

## Then untrack this bundle (you said you'd do this once local)
   git rm -r --cached _claude-migration
   echo "_claude-migration/" >> .gitignore
   # (blobs remain in history; fine for 576K of text. Don't add transcripts here.)

## Stale-path caveat
Some memory notes hardcode `A:\...` paths (launch.json -> target/debug/albedo.exe,
`A:\halation`, etc.). Those are wrong on the new drive. Flag them to Claude and fix as you go.
