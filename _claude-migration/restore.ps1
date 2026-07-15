# restore.ps1 — one-shot Claude Code state restore on a new machine.
# Run from anywhere: it locates the repo from its own location.
#   powershell -ExecutionPolicy Bypass -File .\_claude-migration\restore.ps1
# Then just launch `claude` in the repo.

$ErrorActionPreference = "Stop"

# --- locate things -----------------------------------------------------------
$bundle   = $PSScriptRoot                       # ...\_claude-migration
$repoRoot = Split-Path $bundle -Parent          # the cloned repo root
$claude   = Join-Path $env:USERPROFILE ".claude"

# Bucket name = repo root's absolute path with every non-alphanumeric char -> '-'
# (matches how Claude Code keys per-project state; hyphens map to hyphens.)
$bucket = ($repoRoot -replace '[^a-zA-Z0-9]', '-')
$proj   = Join-Path $claude "projects\$bucket"

Write-Host "Repo root : $repoRoot"
Write-Host "Bucket    : $bucket"
Write-Host "Target    : $proj`n"

# --- global config -----------------------------------------------------------
New-Item -ItemType Directory -Force -Path $claude | Out-Null
Copy-Item (Join-Path $bundle "global\skills")       (Join-Path $claude "skills")       -Recurse -Force
Copy-Item (Join-Path $bundle "global\CLAUDE.md")    (Join-Path $claude "CLAUDE.md")    -Force
if (Test-Path (Join-Path $bundle "global\settings.json")) {
    Copy-Item (Join-Path $bundle "global\settings.json") (Join-Path $claude "settings.json") -Force
}
Write-Host "[ok] global config restored (skills, CLAUDE.md, settings.json)"

# --- project memory ----------------------------------------------------------
$memDest = Join-Path $proj "memory"
New-Item -ItemType Directory -Force -Path $memDest | Out-Null
Copy-Item (Join-Path $bundle "memory\*") $memDest -Recurse -Force
$n = (Get-ChildItem $memDest -File).Count
Write-Host "[ok] $n memory files restored"

# --- optional: transcripts from a USB ----------------------------------------
# If you brought the *.jsonl transcripts, drop them next to this script in a
# 'transcripts' folder and they'll be restored too (enables /resume of old chats).
$tx = Join-Path $bundle "transcripts"
if (Test-Path $tx) {
    Copy-Item (Join-Path $tx "*.jsonl") $proj -Force
    Write-Host "[ok] transcripts restored"
}

Write-Host "`nDone. Launch:  cd `"$repoRoot`"; claude"
