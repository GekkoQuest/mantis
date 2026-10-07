# Owner: deploy-engine
# Builds the per-role container images and starts the local compose cluster: one
# container per role (account, realm, social, matchmaking, persist, ops, the gateway, the toy
# cell host) and PostgreSQL, from deploy/compose/compose.yaml. Prints the service graph when every
# container is healthy (each health check is its node's /ready).
#
# First run only, into deploy/local/ (ignored by git, never committed):
#   keys/      the cluster key, the deploy key pair, the live-data key pair, the operator
#              token, the cluster CA (`mantisd keys --cluster compose-local`); the CA's
#              private key stays here and is never mounted into a container
#   secrets/   the database password and the writer's connection string
#   registry.toml  the registry, filled in (with the CA) and signed with the deploy key
#   certs/     each node's mutual-TLS certificate and key (`mantisd certs`, 7 days), the cell
#              host's game listener chain and the gateway's client-facing chain
#   secrets/bots-login.txt  the bots' login targets and the cluster key (scripts/deploy-bots.ps1)
# -Fresh writes a new registry (a higher serial) and new certificates, without touching
# keys or data.
# -Renew is certificate renewal with no restart: new certificates for every node, the game
# listener and the gateway's client-facing listener from the same CA are written over the
# mounted files; every node takes its new one up within a second (internal RPC connections
# handshake again; game sessions stay on their connection, only new ones get the new chain). The script waits until every
# node reports the renewal on /metrics. Run it before the certificates expire (valid 7
# days; the script warns when fewer than 2 remain).
#
# -BuildOnly builds the images and stops (scripts/deploy-multihost.ps1 -Build uses it).
# -Runtime debian (default) or distroless picks the runtime image every role is built on
# (MANTIS_RUNTIME for compose).
# Images: rust:1.98.1-bookworm, debian:bookworm-slim and gcr.io/distroless/cc-debian12 (pinned by digest in
# deploy/containers/base.Containerfile) and postgres:17-alpine. This script builds; it does
# not pull anything the build does not name. Every published port binds 127.0.0.1:
#   7400/udp  native clients (QUIC), the gateway: the only native game address
#   7401/tcp  legacy clients (TCP), the cell host itself (the legacy protocol has no gateway)
#   7480/tcp  the Ops dashboard (HTTPS, its own network; operator token in
#             deploy/local/keys/operator.token, certificate in deploy/local/out/ops-cert.der)
# Bots: scripts/deploy-bots.ps1 [-Legacy] (logged in: native through the gateway, legacy
#       straight to the cell host; the cell host redeems every entry token)
# Stop: scripts/deploy-down.ps1
param(
    [ValidateSet('debian', 'distroless')][string]$Runtime = 'debian',
    [switch]$Fresh,
    [switch]$Renew,
    [switch]$NoBuild,
    [switch]$BuildOnly,
    [int]$WaitSeconds = 600
)
. "$PSScriptRoot/common.ps1"
$env:MANTIS_RUNTIME = $Runtime

$Local = Join-Path $Root 'deploy/local'
$Compose = 'deploy/compose/compose.yaml'

function Invoke-Mantisd {
    param([string[]]$Arguments)
    Invoke-Checked cargo (@('run', '-q', '-p', 'mantis-deploy', '--bin', 'mantisd', '--') + $Arguments)
}

# Every node (the failover roles' standbys included): its compose service and its health
# port inside the container. The cell host stays last (its game listener is checked too).
$Nodes = @(
    @{ Service = 'persist'; Health = 7605 }, @{ Service = 'account'; Health = 7601 },
    @{ Service = 'realm'; Health = 7602 }, @{ Service = 'social'; Health = 7603 },
    @{ Service = 'matchmaking'; Health = 7604 }, @{ Service = 'ops'; Health = 7606 },
    @{ Service = 'account-2'; Health = 7601 }, @{ Service = 'realm-2'; Health = 7602 },
    @{ Service = 'social-2'; Health = 7603 }, @{ Service = 'matchmaking-2'; Health = 7604 },
    @{ Service = 'ops-2'; Health = 7606 }, @{ Service = 'gateway'; Health = 7630 },
    @{ Service = 'cell-host'; Health = 7620 })
$Gateway = $Nodes[-2]

# A counter from a node's /metrics (0 when absent), read inside its container with
# `mantisd probe` (no shell needed: works on distroless).
function Get-NodeMetric {
    param($Node, [string]$Name)
    $id = (& docker compose -f $Compose ps -q $Node.Service 2>$null | Select-Object -First 1)
    if (-not $id) { return 0 }
    $text = (& docker exec $id mantisd probe "127.0.0.1:$($Node.Health)" /metrics 2>$null) -join "`n"
    $line = ($text -split "`n") | Where-Object { $_ -like "$Name *" } | Select-Object -First 1
    if ($line) { return [int64]($line.Split(' ')[1]) } else { return 0 }
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
# The bots' login targets (every account and realm instance on the services network) and the
# cluster key.
$clusterKey = ([System.IO.File]::ReadAllText((Join-Path $Local 'keys/cluster.key'))).Trim()
Write-Secret (Join-Path $Local 'secrets/bots-login.txt') ("account = 10.77.0.11:7501,10.77.0.21:7501`n" +
    "realm = 10.77.0.12:7502,10.77.0.22:7502`ncluster_key = $clusterKey`n")
# The certificates containers write for clients are regenerated at every start; old ones are
# removed first, since a file a container of another runtime (another uid) wrote cannot be
# overwritten by this one.
New-Item -ItemType Directory -Force (Join-Path $Local 'out') | Out-Null
Get-ChildItem (Join-Path $Local 'out') -File | Remove-Item -Force

# The registry: the live-data public key and a serial filled in, then signed.
$registry = Join-Path $Local 'registry.toml'
# A registry written before the gateway joined the template: written again (a higher serial),
# with every certificate.
if ((Test-Path $registry) -and -not (Select-String -Quiet -SimpleMatch '[instance.gateway-1]' $registry)) {
    Write-Host 'the registry predates the gateway: writing it and the certificates again'
    $Fresh = $true
}
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
$issue = $Fresh -or $Renew -or -not (Test-Path (Join-Path $certs 'gateway.crt'))
# What each node has taken up so far (a renewal is counted when it raises these).
$before = @{}
if ($Renew) {
    foreach ($n in $Nodes) { $before[$n.Service] = Get-NodeMetric $n 'tls_renewals' }
    $before['game'] = Get-NodeMetric $Nodes[-1] 'game_tls_rotations'
    $before['front'] = Get-NodeMetric $Gateway 'gateway_tls_rotations'
}
if ($issue) {
    Invoke-Mantisd @('certs', '--keys', 'deploy/local/keys', '--registry', 'deploy/local/registry.toml',
        '--out', 'deploy/local/certs')
    # The game listener's chain, from the cluster CA, for the address the gateway dials and
    # the name it checks (`[gateway] hosts_name`).
    Invoke-Mantisd @('certs', '--keys', 'deploy/local/keys', '--server', 'game', '--hosts',
        '10.78.0.20,cells.compose.mantis', '--out', 'deploy/local/certs')
    # The gateway's client-facing chain, for the addresses clients dial: the published port on
    # loopback, and its game-network address (the bots inside the cluster).
    Invoke-Mantisd @('certs', '--keys', 'deploy/local/keys', '--server', 'gateway', '--hosts',
        '127.0.0.1,10.78.0.30', '--out', 'deploy/local/certs')
} else {
    $age = (Get-Date) - (Get-Item (Join-Path $certs 'cells-1.crt')).LastWriteTime
    if ($age.TotalDays -gt 5) {
        Write-Host "certificates are $([int]$age.TotalDays) days old (valid 7): run with -Renew" -ForegroundColor Yellow
    }
}
if ($Renew) {
    # No restart: wait until every node has taken its new certificate up.
    $deadline = (Get-Date).AddSeconds(60)
    foreach ($n in $Nodes) {
        while ((Get-NodeMetric $n 'tls_renewals') -le $before[$n.Service]) {
            if ((Get-Date) -gt $deadline) { throw "$($n.Service) did not take its renewed certificate up" }
            Start-Sleep -Milliseconds 500
        }
        Write-Host "  $($n.Service): renewed in place"
    }
    while ((Get-NodeMetric $Nodes[-1] 'game_tls_rotations') -le $before['game']) {
        if ((Get-Date) -gt $deadline) { throw 'the game listener did not take its renewed certificate up' }
        Start-Sleep -Milliseconds 500
    }
    Write-Host '  cell-host game listener: rotated (open sessions kept)'
    while ((Get-NodeMetric $Gateway 'gateway_tls_rotations') -le $before['front']) {
        if ((Get-Date) -gt $deadline) { throw 'the gateway did not take its renewed client certificate up' }
        Start-Sleep -Milliseconds 500
    }
    Write-Host '  gateway client listener: rotated (open sessions kept)'
    Invoke-Checked docker @('compose', '-f', $Compose, 'ps', '--format', 'table {{.Service}}\t{{.Status}}')
    Write-Host 'renewed: every node on its new certificate, no restart'
    exit 0
}

# Images: the shared base (build and runtime stages), then one image per role.
if (-not $NoBuild) {
    Invoke-Checked docker @('build', '-f', 'deploy/containers/base.Containerfile', '--target', 'build',
        '-t', 'mantis/build:dev', '.')
    $stage = if ($Runtime -eq 'distroless') { 'runtime-distroless' } else { 'runtime' }
    Invoke-Checked docker @('build', '-f', 'deploy/containers/base.Containerfile', '--target', $stage,
        '-t', "mantis/runtime-${Runtime}:dev", '.')
    Invoke-Checked docker @('compose', '-f', $Compose, 'build')
}
if ($BuildOnly) { Write-Host "images built on the $Runtime runtime"; exit 0 }

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
  gateway       game 127.0.0.1:7400/udp (clients), health 10.77.0.30:7630 -> realm (entry routes), cell-host 10.78.0.20:7400
  cell-host     rpc 10.77.0.20:7520  health :7620   <- ops (inspector, kick, drain), gateway (game 10.78.0.20:7400); legacy 127.0.0.1:7401/tcp
  postgres      10.77.0.4:5432 (services network only, not published)
'@
Write-Host 'bots:  scripts/deploy-bots.ps1 (through the gateway)   legacy: scripts/deploy-bots.ps1 -Legacy'
Write-Host 'stop:  scripts/deploy-down.ps1 [-Volumes]'
