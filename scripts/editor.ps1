# Owner: client-engine
# Runs the toy client with the editor module loaded (F10 shows and hides it, F11
# toggles the UI preview) against a running server, with the same connection options
# as scripts/client.ps1. The editor edits the package's sources in -Content and recooks
# into -World.
#
# -Ops connects the live inspector to the Ops dashboard of scripts/cluster.ps1
# (read-only, over pinned TLS): its address, the dashboard certificate (the cluster's
# -OpsCertOut), the operator token file (-OpsTokenFile), and the cell to inspect.
# -Font is optional: the editor draws with a built-in fixture font without one.
param(
    [string]$Server = '127.0.0.1:7400',
    [string]$Cert = 'dev-cert.der',
    [string]$Font = '',
    [string]$Token = 'player',
    [string]$World = 'packages/toy/cooked',
    [string]$Content = 'packages/toy/content',
    [string]$Ops = '',
    [string]$OpsCert = 'ops-cert.der',
    [string]$OpsTokenFile = 'ops-token.txt',
    [uint64]$OpsCell = 1,
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = @('run', '-p', 'toy-client')
if ($Release) { $a += '--release' }
$a += @('--', '--server', $Server, '--cert', $Cert, '--token', $Token, '--world', $World,
    '--editor', '--content', $Content)
if ($Ops) {
    $a += @('--ops', $Ops, '--ops-cert', $OpsCert, '--ops-token-file', $OpsTokenFile,
        '--ops-cell', "$OpsCell")
}
if ($Font) { $a += @('--font', $Font) }
Invoke-Checked cargo $a
