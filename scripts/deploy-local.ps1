# Owner: deploy-engine
# Builds the per-role container images and starts the local compose cluster: one
# container per role (account, realm, social, matchmaking, persist, ops, the toy cell host)
# and PostgreSQL, from deploy/compose/compose.yaml. Prints the service graph when every
# container is healthy (each health check is its node's /ready).
#
# First run only, into deploy/local/ (ignored by git, never committed):
#   keys/      the cluster key, the deploy key pair, the live-data key pair, the operator
#              token, the cluster CA (`mantisd keys --cluster compose-local`); the CA's
#              private key stays here and is never mounted into a container
#   secrets/   the database password and the writer's connection string
#   registry.toml  the registry, filled in (with the CA) and signed with the deploy key
#   certs/     each node's mutual-TLS certificate and key (`mantisd certs`, 7 days)
# -Fresh writes a new registry (a higher serial) and new certificates, without touching
# keys or data.
# -Renew is certificate renewal: new certificates for every node from the same CA, then
# each container restarted one at a time, waiting for it to be healthy before the next.
# Run it before the certificates expire (they are valid 7 days; the script warns when
# fewer than 2 remain).
#
# Images: rust:1.98.1-bookworm and debian:bookworm-slim (pinned by digest in
# deploy/containers/base.Containerfile) and postgres:17-alpine. This script builds; it does
# not pull anything the build does not name. Every published port binds 127.0.0.1:
#   7400/udp  native clients (QUIC)       7401/tcp  legacy clients (TCP)
#   7480/tcp  the Ops dashboard (HTTPS, its own network; operator token in
#             deploy/local/keys/operator.token, certificate in deploy/local/out/ops-cert.der)
# Bots: scripts/bots.ps1 -Cert deploy/local/out/toy-cert.der   (add -Legacy for TCP)
# Stop: scripts/deploy-down.ps1
param(
    [switch]$Fresh,
    [switch]$Renew,
    [switch]$NoBuild,
    [int]$WaitSeconds = 600
)
. "$PSScriptRoot/common.ps1"

$Local = Join-Path $Root 'deploy/local'
$Compose = 'deploy/compose/compose.yaml'

function Invoke-Mantisd {
    param([string[]]$Arguments)
    Invoke-Checked cargo (@('run', '-q', '-p', 'mantis-deploy', '--bin', 'mantisd', '--') + $Arguments)
}

function New-Hex {
    param([int]$Bytes)
    $b = New-Object byte[] $Bytes
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($b)
    return (($b | ForEach-Object { $_.ToString('x2') }) -join '')
}

function Write-Secret {
    param([string]$Path, [string]$Text)
    New-Item -ItemType Directory -Force (Split-Path -Parent $Path) | Out-Null
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.ASCIIEncoding))
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) { throw 'docker is not on PATH' }

# Keys and secrets, once.
if (-not (Test-Path (Join-Path $Local 'keys/ca.crt'))) {
    Invoke-Mantisd @('keys', '--out', 'deploy/local/keys', '--cluster', 'compose-local')
}
$pgPassword = Join-Path $Local 'secrets/pg_password'
if (-not (Test-Path $pgPassword)) {
    $password = New-Hex 24
    Write-Secret $pgPassword $password
    Write-Secret (Join-Path $Local 'secrets/pg.conn') "host=10.77.0.4 port=5432 user=mantis dbname=mantis password=$password"
    Write-Host 'secrets: a new database password in deploy/local/secrets (files, never environment variables)'
}
New-Item -ItemType Directory -Force (Join-Path $Local 'out') | Out-Null

# The registry: the live-data public key and a serial filled in, then signed.
$registry = Join-Path $Local 'registry.toml'
if ($Fresh -or -not (Test-Path $registry)) {
    $live = ([System.IO.File]::ReadAllText((Join-Path $Local 'keys/live.pub'))).Trim()
    # The CA certificate as hex DER: the base64 between the PEM armour lines.
    $caPem = [System.IO.File]::ReadAllText((Join-Path $Local 'keys/ca.crt'))
    $caB64 = (($caPem -split "`n") | Where-Object { $_ -notmatch '^-----' } | ForEach-Object { $_.Trim() }) -join ''
    $ca = (([Convert]::FromBase64String($caB64)) | ForEach-Object { $_.ToString('x2') }) -join ''
    $serial = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $body = [System.IO.File]::ReadAllText((Join-Path $Root 'deploy/compose/registry.body.toml'))
    $body = $body.Replace('@SERIAL@', "$serial").Replace('@LIVE_KEY@', $live).Replace('@CA@', $ca)
    Write-Secret (Join-Path $Local 'registry.body.toml') $body
    Invoke-Mantisd @('registry', 'sign', '--key', 'deploy/local/keys/deploy.pk8',
        '--in', 'deploy/local/registry.body.toml', '--out', 'deploy/local/registry.toml')
}

# Certificates: one per node, from the CA the registry carries.
$certs = Join-Path $Local 'certs'
$issue = $Fresh -or $Renew -or -not (Test-Path (Join-Path $certs 'cells-1.crt'))
if ($issue) {
    Invoke-Mantisd @('certs', '--keys', 'deploy/local/keys', '--registry', 'deploy/local/registry.toml',
        '--out', 'deploy/local/certs')
} else {
    $age = (Get-Date) - (Get-Item (Join-Path $certs 'cells-1.crt')).LastWriteTime
    if ($age.TotalDays -gt 5) {
        Write-Host "certificates are $([int]$age.TotalDays) days old (valid 7): run with -Renew" -ForegroundColor Yellow
    }
}
if ($Renew) {
    # Rolling restart: each node reads its certificate at start.
    foreach ($svc in @('persist', 'account', 'realm', 'social', 'matchmaking', 'ops', 'cell-host')) {
        Invoke-Checked docker @('compose', '-f', $Compose, 'restart', $svc)
        Invoke-Checked docker @('compose', '-f', $Compose, 'up', '-d', '--wait', '--wait-timeout', "$WaitSeconds", $svc)
    }
    Write-Host 'renewed: every node restarted on its new certificate'
    exit 0
}

# Images: the shared base (build and runtime stages), then one image per role.
if (-not $NoBuild) {
    Invoke-Checked docker @('build', '-f', 'deploy/containers/base.Containerfile', '--target', 'build',
        '-t', 'mantis/build:dev', '.')
    Invoke-Checked docker @('build', '-f', 'deploy/containers/base.Containerfile', '--target', 'runtime',
        '-t', 'mantis/runtime:dev', '.')
    Invoke-Checked docker @('compose', '-f', $Compose, 'build')
}

# Start, and wait until every container's health check (its /ready) passes. Readiness
# order is the nodes' own: persist before social and Ops, realm before matchmaking, every
# service role before the cell host.
Invoke-Checked docker @('compose', '-f', $Compose, 'up', '-d', '--wait', '--wait-timeout', "$WaitSeconds")

# The service graph: the verified registry, then the containers.
Invoke-Mantisd @('registry', 'verify', '--deploy-key', 'deploy/local/keys/deploy.pub', 'deploy/local/registry.toml')
Invoke-Checked docker @('compose', '-f', $Compose, 'ps', '--format', 'table {{.Service}}\t{{.Status}}\t{{.Ports}}')
Write-Host @'
service graph (one container per role; services network 10.77.0.0/24, internal):
  persist       rpc 10.77.0.5:7505   health :7605   <- cell-host (Push), social (guilds, friends), ops (audit, ledger, live values)
  account       rpc 10.77.0.11:7501  health :7601   <- ops (ban, maintenance), cell-host and realm (sessions)
  realm         rpc 10.77.0.12:7502  health :7602   <- cell-host (cells, tokens, epoch), matchmaking (instances), ops
  social        rpc 10.77.0.13:7503  health :7603   <- cell-host (lines, presence, relays, projections), ops (guilds)
  matchmaking   rpc 10.77.0.14:7504  health :7604   <- cell-host (queues, placements)
  ops           rpc 10.77.0.15:7506  health :7606   <- cell-host (live changes); dashboard https 127.0.0.1:7480 (ops network only)
  cell-host     rpc 10.77.0.20:7520  health :7620   <- ops (inspector, kick, drain); game 127.0.0.1:7400/udp, 127.0.0.1:7401/tcp
  postgres      10.77.0.4:5432 (services network only, not published)
'@
Write-Host 'bots:  scripts/bots.ps1 -Cert deploy/local/out/toy-cert.der [-Legacy]'
Write-Host 'stop:  scripts/deploy-down.ps1 [-Volumes]'
