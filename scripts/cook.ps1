# Owner: client-engine
# Cooks a package with mantis-cook: validates its content sources and writes signed,
# content-addressed bundles to -Out (default: <package>/cooked). Without -Key the
# bundles are signed with a key generated for this run, and its public key is written to
# <out>/keys/dev.pub (the key scripts/serve.ps1 checks by default). -Key names a PKCS#8
# signing key file for a production-signed cook.
param(
    [string]$Package = 'packages/toy',
    [string]$Out = '',
    [string]$Key = '',
    [uint64]$ContentVersion = 0,
    [switch]$Release
)
. "$PSScriptRoot/common.ps1"

$a = @('run', '-p', 'mantis-cook')
if ($Release) { $a += '--release' }
$a += @('--', $Package)
if ($Out) { $a += @('--out', $Out) }
if ($Key) { $a += @('--key', $Key) }
if ($ContentVersion -gt 0) { $a += @('--content-version', "$ContentVersion") }
Invoke-Checked cargo $a
