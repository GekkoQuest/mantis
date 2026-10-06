# Owner: server-engine
# Every gate, as CI runs them: build, test, clippy, fmt, rustdoc (`cargo doc` with
# warnings denied, so a broken doc link fails), then the architecture tests, IDL
# freshness, the module wiring check (mantis-modsync --check), and the game-name scan. Runs them all (or stops at the first failure with -StopOnFailure), prints one
# line per gate and a summary line, and exits non-zero if any failed.
# The test gate relies on the build gate running first: crates/deploy's process tests run
# the toy-server executable built next to mantisd (cargo build --workspace --all-targets).
# No gate needs a database or a container: the PostgreSQL tests skip unless
# MANTIS_TEST_POSTGRES is set, and this script never sets it.
param([switch]$StopOnFailure)
. "$PSScriptRoot/common.ps1"

$results = [ordered]@{}

function Show-Summary {
    $failed = @($results.Keys | Where-Object { -not $results[$_] })
    foreach ($k in $results.Keys) {
        Write-Host ('  {0,-14} {1}' -f $k, $(if ($results[$k]) { 'ok' } else { 'FAILED' }))
    }
    $line = "gates: $($results.Count - $failed.Count) of $($results.Count) passed"
    if ($failed.Count) { $line += "; failed: $($failed -join ', ')" }
    Write-Host $line -ForegroundColor $(if ($failed.Count) { 'Red' } else { 'Green' })
}

function Invoke-Gate {
    param([string]$Name, [scriptblock]$Run)
    Write-Host "== $Name" -ForegroundColor Yellow
    $ok = $false
    try { $ok = ((& $Run) -eq 0) } catch { Write-Host $_ -ForegroundColor Red }
    $results[$Name] = $ok
    if (-not $ok -and $StopOnFailure) { Show-Summary; exit 1 }
}

# Game names are forbidden in the engine and packages (CLAUDE.md rule 1), over the
# same paths as CI. The abbreviation is matched as a whole word: as a bare substring
# it matches ordinary identifiers (FriendBook, GuildBook, sandbox).
function Find-GameNames {
    $pattern = '\bdbo\b|dragon ?ball|wildstar'
    Write-Host "> scan crates packages docs/decisions MANTIS-PLAN.md CLAUDE.md for /$pattern/i" -ForegroundColor Cyan
    $paths = @('crates', 'packages', 'docs/decisions', 'MANTIS-PLAN.md', 'CLAUDE.md') |
        ForEach-Object { Join-Path $Root $_ } | Where-Object { Test-Path $_ }
    $hits = @(Get-ChildItem -Path $paths -Recurse -File | Select-String -Pattern $pattern -List)
    foreach ($h in $hits) { Write-Host "  $($h.Path):$($h.LineNumber)" -ForegroundColor Red }
    return $hits.Count
}

Invoke-Gate 'build'        { Invoke-Logged cargo @('build', '--workspace', '--all-targets') }
Invoke-Gate 'test'         { Invoke-Logged cargo @('test', '--workspace') }
Invoke-Gate 'clippy'       { Invoke-Logged cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') }
Invoke-Gate 'fmt'          { Invoke-Logged cargo @('fmt', '--all', '--', '--check') }
Invoke-Gate 'rustdoc'      {
    $saved = $env:RUSTDOCFLAGS
    $env:RUSTDOCFLAGS = '-D warnings'
    Write-Host '  (RUSTDOCFLAGS=-D warnings)' -ForegroundColor Cyan
    try { Invoke-Logged cargo @('doc', '--workspace', '--no-deps') } finally { $env:RUSTDOCFLAGS = $saved }
}
Invoke-Gate 'architecture' { Invoke-Logged cargo @('test', '-p', 'mantis-testkit', '--test', 'architecture') }
Invoke-Gate 'idl'          { Invoke-Logged cargo @('test', '-p', 'mantis-testkit', '--test', 'idl_codegen') }
Invoke-Gate 'modsync'      { Invoke-Logged cargo @('run', '-q', '-p', 'mantis-modsync', '--', '.', '--check') }
Invoke-Gate 'game-names'   { Find-GameNames }

Show-Summary
if ($results.Values -contains $false) { exit 1 }
