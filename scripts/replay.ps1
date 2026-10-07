# Owner: server-engine
# Replays cell logs with `toy-server replay`, verifying every tick's state hash. -Path
# takes log files or directories (every cell-*.log in them). Every toy subcommand runs
# with the same content: the cooked package (-Cooked, default the checked-in cook,
# verified with -Key, default its development key), so logs from soak, serve, cluster
# and node all replay with the defaults. A log written with other content is refused
# with both content hashes named.
param(
    [Parameter(Mandatory)][string[]]$Path,
    [string]$Cooked = '',
    [string]$Key = '',
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$logs = @(foreach ($p in $Path) {
    $full = if ([System.IO.Path]::IsPathRooted($p)) { $p } else { Join-Path $Root $p }
    if (Test-Path $full -PathType Container) {
        Get-ChildItem $full -Filter 'cell-*.log' | Sort-Object Name | ForEach-Object FullName
    } else { $full }
})
if (-not $logs) { throw "no logs found in $($Path -join ', ')" }
$a = (Get-ToyServerArgs -Release:$Release) + 'replay'
if ($Cooked) { $a += @('--cooked', $Cooked) }
if ($Key) { $a += @('--key', $Key) }
Invoke-Checked cargo ($a + $logs)
