# Owner: server-engine
# Connects headless bots to a running server (scripts/serve.ps1 or scripts/cluster.ps1)
# with `toy-server bots`. Native bots use QUIC and pin the certificate the server wrote
# (-Cert, the server's -CertOut); -Legacy uses the TCP protocol instead. Profiles:
# honest, idle, speedhack (meaningful on -Legacy, where the server corrects it). The
# server must run without -VerifyTokens: bots present no realm entry token.
param(
    [ValidateSet('honest', 'idle', 'speedhack')][string]$Profile = 'honest',
    [uint64]$Count = 8,
    [uint64]$Seconds = 60,
    [string]$Quic = '127.0.0.1:7400',
    [string]$Tcp = '127.0.0.1:7401',
    [string]$Cert = 'dev-cert.der',
    [switch]$Legacy,
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
    [uint64]$Seed = 1,
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('bots', '--profile', $Profile, '--count', "$Count",
    '--seconds', "$Seconds", '--seed', "$Seed", '--cooked', $Cooked, '--key', $Key)
if ($Legacy) { $a += @('--tcp', $Tcp) } else { $a += @('--quic', $Quic, '--cert', $Cert) }
Invoke-Checked cargo $a
