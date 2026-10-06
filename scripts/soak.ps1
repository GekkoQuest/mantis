# Owner: server-engine
# Runs `toy-server soak`: the zone in-process with honest bots on both adapters over a
# simulated 100 ms RTT, 2% loss network. Writes each cell's log (cell-N.log) and the
# per-tick state-hash trace (hashes.txt) to -Out. Replay them with scripts/replay.ps1.
param(
    [string]$Out = 'soak-out',
    [uint64]$Ticks = 18000,
    [uint64]$Bots = 64,
    [uint64]$Seed = 1,
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('soak', '--out', $Out, '--ticks', "$Ticks",
    '--bots', "$Bots", '--seed', "$Seed")
Invoke-Checked cargo $a
