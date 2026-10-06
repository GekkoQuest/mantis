# Owner: server-engine
# Replays cell logs with `toy-server replay`, verifying every tick's state hash. -Path
# takes log files or directories (every cell-*.log in them). Logs from `soak` ran on
# the package manifest, not a cook: they replay with -Content manifest (the default);
# logs from a served zone replay with -Content cooked against the same cook.
param(
    [Parameter(Mandatory)][string[]]$Path,
    [ValidateSet('manifest', 'cooked')][string]$Content = 'manifest',
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
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
if ($Content -eq 'manifest') {
    $a += @('--content', 'manifest')
} else {
    $a += @('--cooked', $Cooked, '--key', $Key)
}
Invoke-Checked cargo ($a + $logs)
