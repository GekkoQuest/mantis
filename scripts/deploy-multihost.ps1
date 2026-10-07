# Owner: deploy-engine
# The multi-host demonstration on one machine: three compose projects that pretend to be
# three machines, meeting only on a shared network by DNS name (deploy/multihost):
#   mantis-svc     every service role, PostgreSQL, `mantisd registry serve`, and the gateway:
#                  the one native game address, 127.0.0.1:7400/udp
#   mantis-host-a  a cell host (cells 1-3, world 0), wan 10.88.0.10, legacy 127.0.0.1:7401/tcp
#   mantis-host-b  a cell host (cells 11-13, world 1), wan 10.88.0.11, legacy 127.0.0.1:7411/tcp
# Every node fetches the signed registry over HTTPS (pinned to the cluster CA) and re-reads
# it every 5 s.
#
# First run only, into deploy/local/multihost (ignored by git): keys and the cluster CA
# (cluster "multihost") and secrets. Every start: the signed registry in published/ (a
# higher serial), every node's certificate (DNS names), the registry server's, each cell
# host's game listener chain (its wan address and the gateway's hosts_name), the gateway's
# client-facing chain, and the bots' login targets.
# Account, realm, social, matchmaking and Ops run an active and a standby instance each. Images are the ones
# scripts/deploy-local.ps1 builds (-Build builds them here first).
#
#   -Publish  signs and publishes the registry again with a higher serial (every node
#             applies it within 5 s, without a restart)
#   -Down     stops all three projects (-Volumes also deletes the database and cell state)
# Bots:  scripts/deploy-bots.ps1 -Multihost (logged in, through the gateway; the realm places
#        each in a world, and the gateway relays it to that world's host)
#        scripts/deploy-bots.ps1 -Multihost -Legacy (logged in, legacy TCP straight to host a)
# The shared network `wan` has the subnet 10.88.0.0/24 (fixed addresses for the cell hosts'
# game listeners, which the gateway dials; Docker hands out the others from 10.88.0.128/25);
# one created otherwise is refused: run -Down first.
param(
    [switch]$Build,
    [switch]$Publish,
    [switch]$Down,
    [switch]$Volumes,
    [ValidateSet('debian', 'distroless')][string]$Runtime = 'debian',
    [int]$WaitSeconds = 600
)
. "$PSScriptRoot/common.ps1"
$env:MANTIS_RUNTIME = $Runtime

$Local = Join-Path $Root 'deploy/local/multihost'
$Dir = 'deploy/multihost'
$Hosts = @(@{ Name = 'a'; Wan = '10.88.0.10'; Tcp = 7401 }, @{ Name = 'b'; Wan = '10.88.0.11'; Tcp = 7411 })
$Subnet = '10.88.0.0/24'
$Dynamic = '10.88.0.128/25'

function Invoke-Mantisd {
    param([string[]]$Arguments)
    Invoke-Checked cargo (@('run', '-q', '-p', 'mantis-deploy', '--bin', 'mantisd', '--') + $Arguments)
}

function Invoke-Host {
    param($h, [string[]]$Arguments)
    $env:MANTIS_HOST = $h.Name
    $env:MANTIS_WAN_IP = $h.Wan
    $env:MANTIS_TCP_PORT = "$($h.Tcp)"
    Invoke-Checked docker (@('compose', '-p', "mantis-host-$($h.Name)", '-f', "$Dir/host.yaml") + $Arguments)
}

function Write-Text {
    param([string]$Path, [string]$Text)
    New-Item -ItemType Directory -Force (Split-Path -Parent $Path) | Out-Null
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.ASCIIEncoding))
}

function Publish-Registry {
    $live = ([System.IO.File]::ReadAllText((Join-Path $Local 'keys/live.pub'))).Trim()
    $caPem = [System.IO.File]::ReadAllText((Join-Path $Local 'keys/ca.crt'))
    $caB64 = (($caPem -split "`n") | Where-Object { $_ -notmatch '^-----' } | ForEach-Object { $_.Trim() }) -join ''
    $ca = (([Convert]::FromBase64String($caB64)) | ForEach-Object { $_.ToString('x2') }) -join ''
    $serial = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $body = [System.IO.File]::ReadAllText((Join-Path $Root "$Dir/registry.body.toml"))
    $body = $body.Replace('@SERIAL@', "$serial").Replace('@LIVE_KEY@', $live).Replace('@CA@', $ca)
    Write-Text (Join-Path $Local 'registry.body.toml') $body
    # Signed beside, then renamed into place: the server never serves half a file.
    Invoke-Mantisd @('registry', 'sign', '--key', 'deploy/local/multihost/keys/deploy.pk8',
        '--in', 'deploy/local/multihost/registry.body.toml', '--out', 'deploy/local/multihost/registry.next')
    Move-Item -Force (Join-Path $Local 'registry.next') (Join-Path $Local 'published/registry.toml')
}

if ($Down) {
    $extra = @('down', '--timeout', '30')
    if ($Volumes) { $extra += '--volumes' }
    foreach ($h in $Hosts) { Invoke-Host $h $extra }
    Invoke-Checked docker (@('compose', '-p', 'mantis-svc', '-f', "$Dir/services.yaml") + $extra)
    $null = Invoke-Logged docker @('network', 'rm', 'mantis_wan')
    exit 0
}

if ($Publish) {
    Publish-Registry
    Write-Host 'published: every node applies the new registry within its refresh (5 s)'
    exit 0
}

# Keys, secrets, certificates and the first registry, once.
if (-not (Test-Path (Join-Path $Local 'keys/ca.crt'))) {
    Invoke-Mantisd @('keys', '--out', 'deploy/local/multihost/keys', '--cluster', 'multihost')
    $password = -join ((1..24) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })
    Write-Text (Join-Path $Local 'secrets/pg_password') $password
    Write-Text (Join-Path $Local 'secrets/pg.conn') "host=postgres port=5432 user=mantis dbname=mantis password=$password"
    New-Item -ItemType Directory -Force (Join-Path $Local 'published') | Out-Null
}
# Every start publishes the template as it is now (a higher serial) and issues every
# instance's certificate from it (7 days), so instances added to the template get theirs.
Publish-Registry
Invoke-Mantisd @('certs', '--keys', 'deploy/local/multihost/keys', '--registry',
    'deploy/local/multihost/published/registry.toml', '--out', 'deploy/local/multihost/certs')
Invoke-Mantisd @('certs', '--keys', 'deploy/local/multihost/keys', '--server', 'registry',
    '--hosts', 'registry.svc.mantis', '--out', 'deploy/local/multihost/certs')
foreach ($h in $Hosts) {
    Invoke-Mantisd @('certs', '--keys', 'deploy/local/multihost/keys', '--server', "game-$($h.Name)",
        '--hosts', "$($h.Wan),cells.multihost.mantis", '--out', 'deploy/local/multihost/certs')
}
Invoke-Mantisd @('certs', '--keys', 'deploy/local/multihost/keys', '--server', 'gateway',
    '--hosts', '127.0.0.1,10.88.0.30', '--out', 'deploy/local/multihost/certs')
# The bots' login targets (every account and realm instance, by registry name) and the
# cluster key.
$clusterKey = ([System.IO.File]::ReadAllText((Join-Path $Local 'keys/cluster.key'))).Trim()
Write-Text (Join-Path $Local 'secrets/bots-login.txt') ("account = account.svc.mantis:7501,account-2.svc.mantis:7501`n" +
    "realm = realm.svc.mantis:7502,realm-2.svc.mantis:7502`ncluster_key = $clusterKey`n")
foreach ($o in @('out-svc', 'out-a', 'out-b')) {
    New-Item -ItemType Directory -Force (Join-Path $Local $o) | Out-Null
    Get-ChildItem (Join-Path $Local $o) -File | Remove-Item -Force
}

if ($Build) {
    $a = @('-File', (Join-Path $PSScriptRoot 'deploy-local.ps1'), '-Runtime', $Runtime, '-BuildOnly')
    Invoke-Checked powershell.exe (@('-NoProfile', '-ExecutionPolicy', 'Bypass') + $a)
}

# The shared network between the "machines": no route out, fixed addresses for the cell
# hosts' game listeners.
# (Windows PowerShell turns a native command's stderr into errors: read it with them off.)
$ErrorActionPreference = 'Continue'
$wan = (& docker network inspect mantis_wan --format '{{range .IPAM.Config}}{{.Subnet}} {{.IPRange}}{{end}}' 2>&1 | Out-String)
$found = $LASTEXITCODE -eq 0
$ErrorActionPreference = 'Stop'
if (-not $found) {
    # Addresses Docker hands out come from the upper half, clear of the fixed ones below it.
    Invoke-Checked docker @('network', 'create', '--internal', '--subnet', $Subnet, '--ip-range', $Dynamic,
        'mantis_wan')
} elseif ("$wan".Trim() -ne "$Subnet $Dynamic") {
    throw "mantis_wan exists without the subnet $Subnet and range $Dynamic (it has '$("$wan".Trim())'): run scripts/deploy-multihost.ps1 -Down first"
}

Invoke-Checked docker @('compose', '-p', 'mantis-svc', '-f', "$Dir/services.yaml", 'up', '-d', '--wait',
    '--wait-timeout', "$WaitSeconds")
foreach ($h in $Hosts) { Invoke-Host $h @('up', '-d', '--wait', '--wait-timeout', "$WaitSeconds") }

Invoke-Mantisd @('registry', 'verify', '--deploy-key', 'deploy/local/multihost/keys/deploy.pub',
    'deploy/local/multihost/published/registry.toml')
Invoke-Checked docker @('ps', '--filter', 'name=mantis-', '--format', 'table {{.Names}}\t{{.Status}}\t{{.Ports}}')
Write-Host 'bots:  scripts/deploy-bots.ps1 -Multihost   (through the gateway, 127.0.0.1:7400/udp)'
Write-Host '       scripts/deploy-bots.ps1 -Multihost -Legacy   (legacy TCP, straight to host a)'
Write-Host 'move:  scripts/deploy-multihost.ps1 -Publish   stop: scripts/deploy-multihost.ps1 -Down [-Volumes]'
