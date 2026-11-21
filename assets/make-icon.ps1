# Rebuilds icon.ico (and logo.png) from logo.svg and logo-small.svg, using
# headless Microsoft Edge to render the SVGs. Run it after editing either SVG:
#
#   powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1
#
# Sizes up to 40 px use the simplified logo-small.svg; larger ones logo.svg.
param(
    [string]$Assets = $PSScriptRoot,
    [string]$Work = (Join-Path $env:TEMP "gitstepwise-icon")
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$edge = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { throw "Microsoft Edge wasn't found; it renders the SVGs." }
New-Item -ItemType Directory -Force $Work | Out-Null

function Render-Svg($svg, $png) {
    # Edge reports progress on stderr, which "Stop" would treat as an error.
    $ErrorActionPreference = "Continue"
    if (Test-Path $png) { Remove-Item $png }
    $url = "file:///$($svg -replace '\\','/')"
    & $edge --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 `
        --window-size=512,512 --screenshot="$png" $url 2>$null | Out-Null
    for ($i = 0; $i -lt 40 -and -not (Test-Path $png); $i++) { Start-Sleep -Milliseconds 250 }
    if (-not (Test-Path $png)) { throw "Edge didn't render $svg" }
}

function Resize-Png($source, $size) {
    $bitmap = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bitmap)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.DrawImage($source, 0, 0, $size, $size)
    $g.Dispose()
    $stream = New-Object System.IO.MemoryStream
    $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    , $stream.ToArray()
}

Render-Svg (Join-Path $Assets "logo.svg") (Join-Path $Work "full.png")
Render-Svg (Join-Path $Assets "logo-small.svg") (Join-Path $Work "small.png")
Copy-Item (Join-Path $Work "full.png") (Join-Path $Assets "logo.png") -Force

$full = [System.Drawing.Image]::FromFile((Join-Path $Work "full.png"))
$small = [System.Drawing.Image]::FromFile((Join-Path $Work "small.png"))
$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$images = foreach ($size in $sizes) {
    $source = if ($size -le 40) { $small } else { $full }
    [pscustomobject]@{ Size = $size; Bytes = (Resize-Png $source $size) }
}
$full.Dispose(); $small.Dispose()

# ICO: a 6-byte header, a 16-byte entry per image, then the PNG data.
$out = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $out
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($image in $images) {
    $dim = if ($image.Size -ge 256) { 0 } else { $image.Size }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$image.Bytes.Length); $w.Write([uint32]$offset)
    $offset += $image.Bytes.Length
}
foreach ($image in $images) { $w.Write($image.Bytes) }
$w.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $Assets "icon.ico"), $out.ToArray())
"Wrote $(Join-Path $Assets 'icon.ico') ($($out.Length) bytes, sizes $($sizes -join ', '))"
