# Owner: server-engine
# Shared by the server-engine scripts: print a command, run it from the workspace
# root, and report its exit code. Dot-source it: `. "$PSScriptRoot/common.ps1"`.
# Nothing here starts containers or pulls images; the PostgreSQL path uses a
# database you already run.

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot

# Prints `> exe args...`, runs it in the workspace root, and returns its exit code.
# Success is the exit code alone. Windows PowerShell 5.1 turns a native command's
# stderr into error records when the caller redirects it (`2>&1`, `*>`); cargo writes
# its progress there, so stderr is merged here and printed as plain text, with errors
# not stopping the script, whether or not the caller redirects.
function Invoke-Logged {
    param([Parameter(Mandatory)][string]$Exe, [string[]]$Arguments = @())
    $shown = ($Arguments | ForEach-Object { if ($_ -match '\s') { "`"$_`"" } else { $_ } }) -join ' '
    Write-Host "> $Exe $shown" -ForegroundColor Cyan
    Push-Location $Root
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { $_.Exception.Message } else { $_ }
        } | Out-Host
        return $LASTEXITCODE
    } finally {
        Pop-Location
    }
}

# Runs a command and throws when it fails.
function Invoke-Checked {
    param([Parameter(Mandatory)][string]$Exe, [string[]]$Arguments = @())
    $code = Invoke-Logged $Exe $Arguments
    if ($code -ne 0) { throw "$Exe exited with $code" }
}

# `cargo run` arguments for toy-server, debug or release, up to the `--`.
function Get-ToyServerArgs {
    param([switch]$Release)
    $a = @('run', '-p', 'toy-server')
    if ($Release) { $a += '--release' }
    return $a + '--'
}
