---
name: reference-graphify-cli-fix
description: Where graphify CLI lives + how it was put on PATH (the hook demands it every session)
metadata: 
  node_type: memory
  type: reference
  originSessionId: 003d3e08-3f0c-49d8-b670-9225763e69ed
---

The project's PreToolUse hooks MANDATE running `graphify query/explain/path` before reading
source, but the CLI was not on PATH → `graphify: command not found`.

**Facts (this machine):**
- graphify (`graphifyy` pkg) is installed under interpreter `C:\Python314\python.exe`.
- Its console script: `C:\Users\bisha\AppData\Roaming\Python\Python314\Scripts\graphify.exe`.
- `graphify-out/.graphify_python` already contains `C:\Python314\python.exe` (correct).

**Fix applied (2026-06-26):**
1. Added the Scripts dir to **User PATH** permanently (`[Environment]::SetEnvironmentVariable(...,'User')`)
   → fixes all future PowerShell/cmd/new-Git-Bash sessions.
2. Wrote a Git Bash shim at `C:\Users\bisha\bin\graphify` (that dir is first on the Bash-tool PATH)
   that `exec`s the real `graphify.exe "$@"` → makes bare `graphify` work in the *current* Bash
   session without a shell restart.

Both verified: `graphify query "..."` returns a scoped subgraph. If it ever breaks again, re-check
the Python314 Scripts path above and re-create the shim. Graph data lives in `graphify-out/`.
