# Install the pinned Azure Artifact Signing tools and write the signing
# metadata, then export SIGNTOOL / SIGN_DLIB / SIGN_METADATA to later CI steps.
# Mirrors lena-mono scripts/client-release/Install-ReleaseTools.ps1 (same
# package versions) with the GitHub Actions credential: the workflow's OIDC
# token through WorkloadIdentityCredential, nothing else.
param([Parameter(Mandatory)][string]$Destination)
$ErrorActionPreference = 'Stop'

foreach ($name in @('ARTIFACT_SIGNING_ACCOUNT', 'ARTIFACT_SIGNING_PROFILE', 'AZURE_CLIENT_ID', 'AZURE_TENANT_ID')) {
    if (![Environment]::GetEnvironmentVariable($name)) { throw "Missing signing configuration: $name" }
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
$Destination = (Resolve-Path $Destination).Path

$nuget = Join-Path $Destination 'nuget.exe'
Invoke-WebRequest 'https://dist.nuget.org/win-x86-commandline/v6.11.1/nuget.exe' -OutFile $nuget
if ((Get-AuthenticodeSignature $nuget).Status -ne 'Valid') { throw 'NuGet signature verification failed.' }
foreach ($package in @(
    @('Microsoft.Windows.SDK.BuildTools', '10.0.26100.7175'),
    @('Microsoft.ArtifactSigning.Client', '1.0.128')
)) {
    & $nuget install $package[0] -Version $package[1] -OutputDirectory $Destination -NonInteractive -DirectDownload -Source 'https://api.nuget.org/v3/index.json'
    if ($LASTEXITCODE -ne 0) { throw "nuget install $($package[0]) failed" }
}
$signTool = Get-ChildItem (Join-Path $Destination 'Microsoft.Windows.SDK.BuildTools.10.0.26100.7175') -Filter signtool.exe -Recurse | Where-Object { $_.Directory.Name -eq 'x64' } | Select-Object -First 1
$dlib = Get-ChildItem (Join-Path $Destination 'Microsoft.ArtifactSigning.Client.1.0.128') -Filter Azure.CodeSigning.Dlib.dll -Recurse | Where-Object { $_.Directory.Name -eq 'x64' } | Select-Object -First 1
if (!$signTool -or !$dlib) { throw 'Pinned signing tools were not found.' }

$metadata = Join-Path $Destination 'signing-metadata.json'
$excluded = @('ManagedIdentityCredential', 'EnvironmentCredential', 'AzureCliCredential', 'SharedTokenCacheCredential', 'VisualStudioCredential', 'VisualStudioCodeCredential', 'AzurePowerShellCredential', 'AzureDeveloperCliCredential', 'InteractiveBrowserCredential')
$json = @{
    Endpoint = 'https://weu.codesigning.azure.net'
    CodeSigningAccountName = $env:ARTIFACT_SIGNING_ACCOUNT
    CertificateProfileName = $env:ARTIFACT_SIGNING_PROFILE
    ExcludeCredentials = $excluded
} | ConvertTo-Json
[IO.File]::WriteAllText($metadata, $json, [Text.UTF8Encoding]::new($false))

"SIGNTOOL=$($signTool.FullName)" >> $env:GITHUB_ENV
"SIGN_DLIB=$($dlib.FullName)" >> $env:GITHUB_ENV
"SIGN_METADATA=$metadata" >> $env:GITHUB_ENV
