# Owner: client-engine
# Runs the toy client against a running server (scripts/serve.ps1 or
# scripts/cluster.ps1, whose defaults these match): QUIC to -Server, pinning the
# certificate the server wrote (-Cert, the server's -CertOut). The cooked world in
# -World streams in and its gameplay bundle hash is announced at the handshake (cook
# first: scripts/cook.ps1); -NoWorld draws a flat placeholder instead.
#
# -Font is optional: with a TTF/OTF file the module screens and client mods are drawn
# (the repository ships no fonts); without one the client plays with no UI. Client
# mods load from -Mods (default: the package's own packages/toy/mods).
param(
    [string]$Server = '127.0.0.1:7400',
    [string]$Cert = 'dev-cert.der',
    [string]$Font = '',
    [string]$Token = 'player',
    [string]$World = 'packages/toy/cooked',
    [switch]$NoWorld,
    [string]$Mods = '',
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = @('run', '-p', 'toy-client')
if ($Release) { $a += '--release' }
$a += @('--', '--server', $Server, '--cert', $Cert, '--token', $Token)
if ($NoWorld) { $a += '--no-world' } else { $a += @('--world', $World) }
if ($Font) { $a += @('--font', $Font) }
if ($Mods) { $a += @('--mods', $Mods) }
Invoke-Checked cargo $a
