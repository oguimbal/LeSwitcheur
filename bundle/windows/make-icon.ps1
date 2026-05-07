# Regenerate bundle/windows/AppIcon.ico from brand/logo-1024.png.
#
# Used as the SetupIconFile by bundle/windows/installer.iss (the wizard's
# title-bar/tray icon and the icon embedded into the produced setup.exe).
# Re-run only when the brand artwork changes; the resulting AppIcon.ico is
# committed so that CI and local installer builds don't depend on this
# script or on ImageMagick being installed.
#
# Format: classic ICO container (ICONDIR + ICONDIRENTRY[N]) with PNG-encoded
# payloads (Vista+ supports this; Inno Setup 6 reads PNG-payload ICOs fine).
# Sizes match what crates/switcheur/build.rs embeds into the .exe itself.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$Root      = (Resolve-Path "$PSScriptRoot\..\..").Path
$SourcePng = Join-Path $Root 'brand\logo-1024.png'
$OutIco    = Join-Path $PSScriptRoot 'AppIcon.ico'
$Sizes     = @(16, 32, 48, 64, 128, 256)

if (-not (Test-Path $SourcePng)) {
    throw "source artwork missing: $SourcePng"
}

Write-Host ">> Loading $SourcePng"
$srcBitmap = [System.Drawing.Bitmap]::FromFile($SourcePng)
try {
    $pngPayloads = @()
    foreach ($size in $Sizes) {
        Write-Host ">> Rendering ${size}x${size}"
        $resized = New-Object System.Drawing.Bitmap $size, $size
        try {
            $g = [System.Drawing.Graphics]::FromImage($resized)
            try {
                $g.InterpolationMode  = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $g.SmoothingMode      = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
                $g.PixelOffsetMode    = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
                $g.DrawImage($srcBitmap, 0, 0, $size, $size)
            } finally {
                $g.Dispose()
            }
            $ms = New-Object System.IO.MemoryStream
            try {
                $resized.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
                $pngPayloads += ,$ms.ToArray()
            } finally {
                $ms.Dispose()
            }
        } finally {
            $resized.Dispose()
        }
    }
} finally {
    $srcBitmap.Dispose()
}

Write-Host ">> Writing $OutIco"
$fs = [System.IO.File]::Create($OutIco)
try {
    $bw = New-Object System.IO.BinaryWriter $fs
    try {
        # ICONDIR
        $bw.Write([uint16]0)                  # reserved
        $bw.Write([uint16]1)                  # type = icon
        $bw.Write([uint16]$Sizes.Length)      # count

        $headerBytes = 6 + (16 * $Sizes.Length)
        $offset = $headerBytes
        for ($i = 0; $i -lt $Sizes.Length; $i++) {
            $size  = $Sizes[$i]
            $bytes = $pngPayloads[$i]
            $w = if ($size -ge 256) { [byte]0 } else { [byte]$size }  # 0 means 256
            $h = $w
            $bw.Write([byte]$w)               # width
            $bw.Write([byte]$h)               # height
            $bw.Write([byte]0)                # colour count (0 = >=256 or PNG)
            $bw.Write([byte]0)                # reserved
            $bw.Write([uint16]1)              # planes
            $bw.Write([uint16]32)             # bitcount
            $bw.Write([uint32]$bytes.Length)  # bytes in resource
            $bw.Write([uint32]$offset)        # offset
            $offset += $bytes.Length
        }

        foreach ($bytes in $pngPayloads) {
            $bw.Write($bytes)
        }
    } finally {
        $bw.Dispose()
    }
} finally {
    $fs.Dispose()
}

$icoInfo = Get-Item $OutIco
Write-Host (">> done: {0} ({1:N0} bytes)" -f $OutIco, $icoInfo.Length)
