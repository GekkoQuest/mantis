# Owner: client-engine
# Runs the headless end-to-end suites (packages/toy/e2e): the native client against
# the toy server, the cooked and streamed world, the content hash, client mods, the
# editor's inspector, and the town-300 reference scene. Metric lines
# (MANTIS-METRIC ...) are printed with --nocapture.
#
# -Profile debug (default) runs every suite in a debug build; release runs them in a
# release build, which is where the frame-time and hand-off budget rows are measured;
# both runs debug, then release. -Test runs one suite (for example town_300).
# A release run removes <target dir>/release afterwards to keep the build cache small,
# unless -KeepRelease.
param(
    [ValidateSet('debug', 'release', 'both')][string]$Profile = 'debug',
    [string]$Test = '',
    [switch]$KeepRelease
)
. "$PSScriptRoot/common.ps1"

function Get-E2eArgs([switch]$Release) {
    $a = @('test')
    if ($Release) { $a += '--release' }
    $a += @('-p', 'toy-e2e')
    if ($Test) { $a += @('--test', $Test) }
    return $a + @('--', '--nocapture')
}

if ($Profile -ne 'release') {
    Invoke-Checked cargo (Get-E2eArgs)
}
if ($Profile -ne 'debug') {
    try {
        Invoke-Checked cargo (Get-E2eArgs -Release)
    } finally {
        if (-not $KeepRelease) {
            $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
            $release = Join-Path (Join-Path $Root $target) 'release'
            if ([System.IO.Path]::IsPathRooted($target)) { $release = Join-Path $target 'release' }
            if (Test-Path $release) {
                Write-Host "> remove $release" -ForegroundColor Cyan
                Remove-Item -Recurse -Force $release
            }
        }
    }
}
