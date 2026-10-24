# Sign files with Oganbo's Azure Artifact Signing identity (same identity as
# Lena), then check the result. `-VerifyOnly` only checks. Run from GitHub
# Actions after signing-tools.ps1; Inno Setup calls it for the setup and the
# uninstaller (SignTool=azuresign in installer.iss).
#
# Port of lena-mono scripts/client-release/Invoke-ArtifactSigning.ps1 (fresh
# OIDC assertion per file: SignTool starts a new credential each time) and
# Sign-ClientFile.ps1 (Test-OganboSignature).
param(
    [Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Files,
    [switch]$VerifyOnly
)
$ErrorActionPreference = 'Stop'

# Artifact Signing certificates rotate every few days: pin the identity EKU,
# never a thumbprint. Keep in sync with
# crates/switcheur-platform/src/windows/authenticode.rs.
$IdentityEku = '1.3.6.1.4.1.311.97.520439368.510019483.389024596.527382922'

function Test-OganboSignature([string]$Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or $null -eq $signature.TimeStamperCertificate) { return $false }
    $eku = $signature.SignerCertificate.Extensions | Where-Object { $_.Oid.Value -eq '2.5.29.37' }
    if ($null -eq $eku) { return $false }
    $usages = @($eku.EnhancedKeyUsages | ForEach-Object Value)
    return $IdentityEku -in $usages -and '1.3.6.1.4.1.311.97.1.0' -in $usages
}

function Invoke-Signing([string]$File) {
    foreach ($name in @('SIGNTOOL', 'SIGN_DLIB', 'SIGN_METADATA', 'AZURE_CLIENT_ID', 'AZURE_TENANT_ID', 'RUNNER_TEMP', 'ACTIONS_ID_TOKEN_REQUEST_URL', 'ACTIONS_ID_TOKEN_REQUEST_TOKEN')) {
        if (![Environment]::GetEnvironmentVariable($name)) { throw "Missing signing configuration: $name" }
    }
    $separator = if ($env:ACTIONS_ID_TOKEN_REQUEST_URL.Contains('?')) { '&' } else { '?' }
    $requestUrl = $env:ACTIONS_ID_TOKEN_REQUEST_URL + $separator + 'audience=api%3A%2F%2FAzureADTokenExchange'
    try {
        $assertion = (Invoke-RestMethod -Uri $requestUrl -Headers @{ Authorization = "Bearer $env:ACTIONS_ID_TOKEN_REQUEST_TOKEN" } -TimeoutSec 30).value
        if ($assertion -cnotmatch '^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$') { throw 'Invalid OIDC response.' }
    } catch {
        # HTTP errors can include request headers or response bodies.
        throw 'Could not obtain the GitHub OIDC assertion for Artifact Signing.'
    }
    Write-Host "::add-mask::$assertion"
    $tokenFile = Join-Path $env:RUNNER_TEMP ("leswitcheur-signing-oidc-{0}.jwt" -f [Guid]::NewGuid())
    $previous = $env:AZURE_FEDERATED_TOKEN_FILE
    try {
        [IO.File]::WriteAllText($tokenFile, $assertion, [Text.UTF8Encoding]::new($false))
        $assertion = $null
        $env:AZURE_FEDERATED_TOKEN_FILE = $tokenFile
        & $env:SIGNTOOL sign /fd SHA256 /td SHA256 /tr 'http://timestamp.acs.microsoft.com' /dlib $env:SIGN_DLIB /dmdf $env:SIGN_METADATA $File
        if ($LASTEXITCODE -ne 0) { throw "signtool sign failed: $File" }
    } finally {
        $env:AZURE_FEDERATED_TOKEN_FILE = $previous
        if (Test-Path -LiteralPath $tokenFile) { Remove-Item -LiteralPath $tokenFile -Force }
    }
}

foreach ($file in $Files) {
    $file = (Resolve-Path -LiteralPath $file).Path
    if (!$VerifyOnly) { Invoke-Signing $file }
    if (!(Test-OganboSignature $file)) { throw "Missing timestamp or unexpected publisher: $file" }
    Write-Host "Signed by Oganbo: $file"
}
