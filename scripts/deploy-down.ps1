# Owner: deploy-engine
# Stops the local compose cluster started by scripts/deploy-local.ps1. Every container gets
# SIGTERM and stop_grace_period to drain: cell hosts snapshot every cell and flush their
# outcomes to the writer, service roles answer what is in flight and close their
# connections. -Volumes also deletes the database and the cell state (snapshots and logs);
# keys, secrets and the registry in deploy/local stay.
param([switch]$Volumes)
. "$PSScriptRoot/common.ps1"

$a = @('compose', '-f', 'deploy/compose/compose.yaml', 'down', '--timeout', '30')
if ($Volumes) {
    Write-Host 'deleting the database and cell state volumes' -ForegroundColor Yellow
    $a += '--volumes'
}
Invoke-Checked docker $a
