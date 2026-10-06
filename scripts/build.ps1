# Owner: server-engine
# Builds the whole workspace, every target. -Release builds the release profile;
# remove it afterwards (`<target dir>/release`) to keep the target directory small.
param([switch]$Release)
. "$PSScriptRoot/common.ps1"

$a = @('build', '--workspace', '--all-targets')
if ($Release) { $a += '--release' }
Invoke-Checked cargo $a
