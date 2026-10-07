# Owner: deploy-engine
# Headless bots against a container deployment. Each bot logs in through the account and realm
# roles with the gateway's certificate (the gateway's login flow: account bot-<seed>-<n>), and
# presents the entry token it gets: the cell host redeems every token, once. Native bots dial
# only the gateway, trusting the cluster CA. They run in a container on the deployment's own
# networks (the roles are never published), from the cell-host image, which carries the toy
# package's binary. Nothing is published. The login targets (every account and realm
# instance, and the cluster key) are a secret file the deployment's script writes.
#
#   scripts/deploy-bots.ps1 [-Count 8] [-Seconds 30] [-Seed 1]   the local compose cluster
#   scripts/deploy-bots.ps1 -Multihost                         the multi-host demonstration
#   -Legacy   the legacy (plaintext TCP) adapter instead, which has no gateway: straight to the
#             cell host's legacy port (compose; host a in the multi-host demonstration)
param(
    [int]$Count = 8,
    [int]$Seconds = 30,
    [int]$Seed = 1,
    [switch]$Multihost,
    [switch]$Legacy
)
. "$PSScriptRoot/common.ps1"

if ($Multihost) {
    $Compose = @('compose', '-p', 'mantis-svc', '-f', 'deploy/multihost/services.yaml')
    $LegacyAt = '10.88.0.10:7401'
} else {
    $Compose = @('compose', '-f', 'deploy/compose/compose.yaml')
    $LegacyAt = '10.78.0.20:7401'
}
$extra = @('--count', "$Count", '--seconds', "$Seconds", '--seed', "$Seed")
if ($Legacy) {
    # `toy-server bots --tcp` takes an IP and port: host a's address on `wan` in the multi-host
    # demonstration.
    $extra += @('--tcp', $LegacyAt)
}
Invoke-Checked docker ($Compose + @('--profile', 'bots', 'run', '--rm', 'bots') + $extra)
