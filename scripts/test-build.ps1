# Test a Windows release build of LeSwitcheur.
#
# Rebuilds in release mode and launches the resulting .exe. Saved settings
# are preserved across runs by default - pass -Reset to wipe them and
# exercise the first-launch flow (onboarding wizard, default config). Pass
# -Attach to tail the log file in the foreground so messages stream to the
# terminal (the release exe runs with windows_subsystem = "windows" so its
# stdout/stderr are detached - only the log file is observable).
#
# Usage: .\scripts\test-build.ps1 [-Reset] [-Attach]

[CmdletBinding()]
param(
    [switch]$Reset,
    [switch]$Attach,
    [switch]$Help
)

if ($Help) {
    Get-Content $PSCommandPath | Select-Object -First 11 |
        ForEach-Object { $_ -replace '^# ?', '' }
    exit 0
}

$ErrorActionPreference = 'Stop'

$Root = Resolve-Path "$PSScriptRoot\.."
$Exe = Join-Path $Root 'target\release\switcheur.exe'
$ConfigDir = Join-Path $env:APPDATA 'gmbl\LeSwitcheur'
$LocalDir = Join-Path $env:LOCALAPPDATA 'fr.gmbl.LeSwitcheur'
$LogFile = Join-Path $LocalDir 'logs\switcheur.log'

# `cargo` may not be on PATH for non-interactive PowerShell sessions even when
# rustup installed it for the user. Stick the cargo bin dir up front so the
# script works regardless of how it's launched.
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

Write-Host '>> Killing any running instance'
Get-Process -Name switcheur -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "   stopping PID $($_.Id)"
    Stop-Process -Id $_.Id -Force
}

if ($Reset) {
    Write-Host '>> --reset: wiping saved settings + cache + logs'
    foreach ($p in @($ConfigDir, $LocalDir)) {
        if (Test-Path $p) {
            Remove-Item -Path $p -Recurse -Force
            Write-Host "   removed $p"
        }
    }
}

# Truncate the log file so we only see what this run produces. Created on
# first write by the tracing subscriber if it doesn't exist.
if (Test-Path $LogFile) {
    Clear-Content -Path $LogFile
}

Write-Host '>> Building release'
Set-Location $Root
cargo build --release -p switcheur
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed (exit $LASTEXITCODE)"
    exit $LASTEXITCODE
}

if (-not (Test-Path $Exe)) {
    Write-Error "expected $Exe after build"
    exit 1
}

$exeInfo = Get-Item $Exe
Write-Host (">> {0} ({1:N1} MB, {2})" -f $Exe, ($exeInfo.Length / 1MB), $exeInfo.LastWriteTime)

Write-Host '>> Launching detached'
Start-Process -FilePath $Exe

if ($Attach) {
    # Wait briefly for the process to create the log file on first run, then
    # tail it. The log path is fixed (`%LOCALAPPDATA%\fr.gmbl.LeSwitcheur\
    # logs\switcheur.log`) so we don't need to discover it.
    $deadline = (Get-Date).AddSeconds(5)
    while (-not (Test-Path $LogFile) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path $LogFile)) {
        Write-Warning "log file did not appear at $LogFile within 5s"
        exit 0
    }
    Write-Host ">> Tailing $LogFile (Ctrl+C to stop; the app keeps running)"
    Get-Content -Path $LogFile -Wait
}
