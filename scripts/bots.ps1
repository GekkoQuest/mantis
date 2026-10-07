# Owner: server-engine
# Connects headless bots to a running server (scripts/serve.ps1 or scripts/cluster.ps1)
# with `toy-server bots`. Native bots use QUIC and pin the certificate the server wrote
# (-Cert, the server's -CertOut), or verify its chain against a CA bundle (-Ca, PEM) for
# the address they dial; -Legacy uses the TCP protocol instead. Profiles:
# honest, idle, speedhack (meaningful on -Legacy, where the server corrects it).
#
# Against a cluster running -VerifyTokens, pass -Login with the file `cluster.ps1`
# wrote (-LoginOut): each bot logs in through the account and realm roles as the
# gateway would (account bot-<seed>-<n>), joins the cell the realm placed it in, and
# presents its entry token. Against a cluster running mutual TLS, -TlsCa, -TlsCert and
# -TlsKey (PEM files) give the bots a gateway certificate. Without -Login the bots
# present no entry token, so the server must run without -VerifyTokens.
#
# -Gateway ADDR sends every native bot to a cluster's gateway (`cluster.ps1 -Gateway`)
# whatever its placement; pin the gateway's certificate with -Cert (its -GatewayCertOut)
# or verify it with -Ca. The gateway routes by entry token, so use -Login with it.
param(
    [ValidateSet('honest', 'idle', 'speedhack')][string]$Profile = 'honest',
    [uint64]$Count = 8,
    [uint64]$Seconds = 60,
    [string]$Quic = '127.0.0.1:7400',
    [string]$Tcp = '127.0.0.1:7401',
    [string]$Cert = 'dev-cert.der',
    [string]$Ca = '',
    [switch]$Legacy,
    [string]$Cooked = 'packages/toy/cooked',
    [string]$Key = 'packages/toy/cooked/keys/dev.pub',
    [uint64]$Seed = 1,
    [string]$Login = '',
    [string]$Gateway = '',
    [string]$TlsCa = '',
    [string]$TlsCert = '',
    [string]$TlsKey = '',
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = (Get-ToyServerArgs -Release:$Release) + @('bots', '--profile', $Profile, '--count', "$Count",
    '--seconds', "$Seconds", '--seed', "$Seed", '--cooked', $Cooked, '--key', $Key)
if ($Legacy) {
    $a += @('--tcp', $Tcp)
} elseif ($Ca) {
    $a += @('--quic', $Quic, '--ca', $Ca)
} else {
    $a += @('--quic', $Quic, '--cert', $Cert)
}
if ($Login) { $a += @('--login', $Login) }
if ($Gateway) {
    if ($Legacy) { throw '-Gateway is for native bots: the legacy protocol connects to a cell host directly' }
    $a += @('--gateway', $Gateway)
}
$tls = @($TlsCa, $TlsCert, $TlsKey) | Where-Object { $_ }
if ($tls.Count -eq 3) {
    $a += @('--tls-ca', $TlsCa, '--tls-cert', $TlsCert, '--tls-key', $TlsKey)
} elseif ($tls.Count -ne 0) {
    throw '-TlsCa, -TlsCert and -TlsKey go together'
}
Invoke-Checked cargo $a
