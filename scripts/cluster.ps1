# Owner: server-engine
# Runs `toy-server cluster`: serve plus every service role in one process (account,
# realm, social, matchmaking, the persistence writer, Ops and its HTTPS dashboard on
# -Ops). The operator token is written to -OpsTokenFile, the dashboard's certificate
# to -OpsCertOut.
#
# The store is in memory unless -Postgres names a connection string to a PostgreSQL
# you already run (for example "host=localhost user=mantis dbname=mantis"). This
# script never starts a container or pulls an image.
param(
    [string]$Quic = '127.0.0.1:7400',
    [string]$Tcp = '127.0.0.1:7401',
    [string]$Ops = '127.0.0.1:7443',
    [string]$OpsTokenFile = 'ops-token.txt',
    [string]$OpsCertOut = 'ops-cert.der',
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
    [string]$CertOut = 'dev-cert.der',
    [string]$Postgres = '',
    [uint64]$Seed = 1,
    [uint64]$Ticks = 0,
    [string]$Snapshots = '',
    [switch]$VerifyTokens,
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('cluster', '--quic', $Quic, '--tcp', $Tcp,
    '--cooked', $Cooked, '--key', $Key, '--cert-out', $CertOut, '--seed', "$Seed",
    '--ops', $Ops, '--ops-token-file', $OpsTokenFile, '--ops-cert-out', $OpsCertOut)
if ($Postgres) {
    Write-Host 'store: PostgreSQL at the given connection (an existing database; nothing is pulled)'
    $a += @('--postgres', $Postgres)
} else {
    Write-Host 'store: in memory (pass -Postgres CONN to use an existing PostgreSQL)'
}
if ($Ticks -gt 0) { $a += @('--ticks', "$Ticks") }
if ($Snapshots) { $a += @('--snapshots', $Snapshots) }
if ($VerifyTokens) { $a += '--verify-tokens' }
Invoke-Checked cargo $a
