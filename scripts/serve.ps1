# Owner: server-engine
# Runs `toy-server serve` on the cooked package: native clients over QUIC, legacy
# clients over TCP, the zone on the wall clock. The server refuses to start unless the
# cooked bundle verifies against -Key (cook first: scripts/cook.ps1). The certificate
# native clients and bots pin is written to -CertOut.
param(
    [string]$Quic = '127.0.0.1:7400',
    [string]$Tcp = '127.0.0.1:7401',
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
    [string]$CertOut = 'dev-cert.der',
    [uint64]$Seed = 1,
    [uint64]$Ticks = 0,
    [string]$Snapshots = '',
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('serve', '--quic', $Quic, '--tcp', $Tcp,
    '--cooked', $Cooked, '--key', $Key, '--cert-out', $CertOut, '--seed', "$Seed")
if ($Ticks -gt 0) { $a += @('--ticks', "$Ticks") }
if ($Snapshots) { $a += @('--snapshots', $Snapshots) }
Invoke-Checked cargo $a
