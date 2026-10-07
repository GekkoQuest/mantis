# Owner: server-engine
# Runs `toy-server cluster`: serve plus every service role in one process (account,
# realm, social, matchmaking, the persistence writer, Ops and its HTTPS dashboard on
# -Ops). The operator token is written to -OpsTokenFile, the dashboard's certificate
# to -OpsCertOut, and where load bots log in to -LoginOut (for `bots.ps1 -Login`).
#
# The store is in memory unless -Postgres names a connection string to a PostgreSQL
# you already run (for example "host=localhost user=mantis dbname=mantis"). This
# script never starts a container or pulls an image.
#
# -Gateway ADDR runs the gateway in front of the game port: the one address clients
# connect to (it routes each entry token through the realm and relays the session to its
# cell host). Its certificate is -GatewayCert / -GatewayKey (PEM, watched), or a
# development one written to -GatewayCertOut for bots to pin (`bots.ps1 -Gateway`).
param(
    [string]$Quic = '127.0.0.1:7400',
    [string]$Tcp = '127.0.0.1:7401',
    [string]$Ops = '127.0.0.1:7443',
    [string]$OpsTokenFile = 'ops-token.txt',
    [string]$OpsCertOut = 'ops-cert.der',
    [string]$LoginOut = 'login.txt',
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
    [string]$CertOut = 'dev-cert.der',
    [string]$Postgres = '',
    [uint64]$Seed = 1,
    [uint64]$Ticks = 0,
    [string]$Snapshots = '',
    [switch]$VerifyTokens,
    [string]$Gateway = '',
    [string]$GatewayCert = '',
    [string]$GatewayKey = '',
    [string]$GatewayCertOut = 'gateway-cert.der',
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('cluster', '--quic', $Quic, '--tcp', $Tcp,
    '--cooked', $Cooked, '--key', $Key, '--cert-out', $CertOut, '--seed', "$Seed",
    '--ops', $Ops, '--ops-token-file', $OpsTokenFile, '--ops-cert-out', $OpsCertOut,
    '--login-out', $LoginOut)
if ($Postgres) {
    Write-Host 'store: PostgreSQL at the given connection (an existing database; nothing is pulled)'
    $a += @('--postgres', $Postgres)
} else {
    Write-Host 'store: in memory (pass -Postgres CONN to use an existing PostgreSQL)'
}
if ($Ticks -gt 0) { $a += @('--ticks', "$Ticks") }
if ($Snapshots) { $a += @('--snapshots', $Snapshots) }
if ($VerifyTokens) { $a += '--verify-tokens' }
if ($Gateway) {
    $a += @('--gateway', $Gateway, '--gateway-cert-out', $GatewayCertOut)
    if ($GatewayCert -or $GatewayKey) {
        if (-not ($GatewayCert -and $GatewayKey)) { throw '-GatewayCert and -GatewayKey go together' }
        $a += @('--gateway-cert', $GatewayCert, '--gateway-key', $GatewayKey)
    }
}
Invoke-Checked cargo $a
