# Generates every icon form from branding/newfoundsync-logo.png.
#
#   - Detects the circular emblem's bounding box (color-keyed against the cream
#     canvas), crops it square, masks the corners transparent (feathered).
#   - Emits icon-source.png (512 master) + icon-{16,32,48,64,128,256}.png.
#   - Emits PWA icons: icon-512.png ("any") + icon-512-maskable.png (padded, "maskable").
#   - Packs icon.ico (PNG-encoded entries; Windows 10/11 reads these natively).
#
# Re-run whenever newfoundsync-logo.png changes:  ./gen-icons.ps1
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

$root    = $PSScriptRoot
$srcPath = Join-Path $root 'newfoundsync-logo.png'
$src     = [System.Drawing.Bitmap]::FromFile($srcPath)
$W = $src.Width; $H = $src.Height
Write-Host "source: $W x $H"

# --- read pixels (normalize to 32bpp BGRA) ---
$rect = New-Object System.Drawing.Rectangle 0, 0, $W, $H
$data = $src.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$stride = $data.Stride
$buf = New-Object byte[] ($stride * $H)
[System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buf, 0, $buf.Length)
$src.UnlockBits($data)

# Cream canvas sampled from the corner.
$cB = $buf[0]; $cG = $buf[1]; $cR = $buf[2]
Write-Host "canvas BGR: $cB $cG $cR"

# --- bounding box of the emblem (pixels far from the canvas color) ---
$T = 48
$minX = $W; $minY = $H; $maxX = 0; $maxY = 0
for ($y = 0; $y -lt $H; $y += 2) {
    $rowoff = $y * $stride
    for ($x = 0; $x -lt $W; $x += 2) {
        $o = $rowoff + $x * 4
        $d = [math]::Abs($buf[$o] - $cB) + [math]::Abs($buf[$o + 1] - $cG) + [math]::Abs($buf[$o + 2] - $cR)
        if ($d -gt $T) {
            if ($x -lt $minX) { $minX = $x }
            if ($x -gt $maxX) { $maxX = $x }
            if ($y -lt $minY) { $minY = $y }
            if ($y -gt $maxY) { $maxY = $y }
        }
    }
}
Write-Host "emblem bbox: ($minX,$minY)-($maxX,$maxY)"

# Fail loudly rather than silently emitting a mis-cropped icon.
if ($maxX -le $minX -or $maxY -le $minY) {
    throw "emblem bbox detection failed (nothing differed from the canvas color) - check the source background or threshold T=$T."
}
$coverX = ($maxX - $minX) / [double]$W
$coverY = ($maxY - $minY) / [double]$H
if ($coverX -gt 0.97 -and $coverY -gt 0.97) {
    Write-Warning "emblem bbox covers ~the whole canvas ($([math]::Round($coverX*100))% x $([math]::Round($coverY*100))%); the crop is likely wrong (logo may bleed to the edges)."
}

# Square crop centered on the bbox.
$bw = $maxX - $minX; $bh = $maxY - $minY
$side = [math]::Max($bw, $bh)
$ccx = [int](($minX + $maxX) / 2); $ccy = [int](($minY + $maxY) / 2)
$x0 = [math]::Max(0, $ccx - [int]($side / 2))
$y0 = [math]::Max(0, $ccy - [int]($side / 2))
if ($x0 + $side -gt $W) { $side = $W - $x0 }
if ($y0 + $side -gt $H) { $side = $H - $y0 }
$cropRect = New-Object System.Drawing.Rectangle $x0, $y0, $side, $side
$cropped = $src.Clone($cropRect, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
Write-Host "crop: $x0,$y0 $side x $side"

function Scale-To($srcBmp, $size) {
    $dst = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($dst)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($srcBmp, (New-Object System.Drawing.Rectangle 0, 0, $size, $size))
    $g.Dispose()
    return $dst
}

# 512 master, then feathered circular alpha mask.
$master = Scale-To $cropped 512
$mw = $master.Width
$mrect = New-Object System.Drawing.Rectangle 0, 0, $mw, $mw
$md = $master.LockBits($mrect, [System.Drawing.Imaging.ImageLockMode]::ReadWrite, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$mstride = $md.Stride
$mbuf = New-Object byte[] ($mstride * $mw)
[System.Runtime.InteropServices.Marshal]::Copy($md.Scan0, $mbuf, 0, $mbuf.Length)
$cx = ($mw - 1) / 2.0; $rad = $mw / 2.0
for ($y = 0; $y -lt $mw; $y++) {
    $ro = $y * $mstride
    $dy = $y - $cx
    for ($x = 0; $x -lt $mw; $x++) {
        $dx = $x - $cx
        $dist = [math]::Sqrt($dx * $dx + $dy * $dy)
        $o = $ro + $x * 4
        if ($dist -gt $rad) {
            $mbuf[$o + 3] = 0
        }
        elseif ($dist -gt $rad - 2.0) {
            $f = ($rad - $dist) / 2.0
            $mbuf[$o + 3] = [byte]([math]::Round($mbuf[$o + 3] * $f))
        }
    }
}
[System.Runtime.InteropServices.Marshal]::Copy($mbuf, 0, $md.Scan0, $mbuf.Length)
$master.UnlockBits($md)
$master.Save((Join-Path $root 'icon-source.png'), [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "wrote icon-source.png (512)"

# --- PWA icons (consumed by the web client manifest) ---
# 512 "any": the masked circular master verbatim - crisp on Android/desktop installs.
$master.Save((Join-Path $root 'icon-512.png'), [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "wrote icon-512.png (512)"

# 512 "maskable": badge centered at ~78% on a dark square so Android's adaptive-icon mask
# (circle/squircle) crops the padding, never the emblem (the safe zone is the inner 80%).
# The fill matches the manifest background_color (#0e1116).
$maskable = New-Object System.Drawing.Bitmap(512, 512, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$mg = [System.Drawing.Graphics]::FromImage($maskable)
$mg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$mg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$mg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
$mg.Clear([System.Drawing.ColorTranslator]::FromHtml('#0e1116'))
$badge = 400; $boff = [int](((512 - $badge) / 2))
$mg.DrawImage($master, (New-Object System.Drawing.Rectangle $boff, $boff, $badge, $badge))
$mg.Dispose()
$maskable.Save((Join-Path $root 'icon-512-maskable.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$maskable.Dispose()
Write-Host "wrote icon-512-maskable.png (512, maskable)"

# Size variants (downscale the masked master so the alpha edge anti-aliases).
$sizes = 16, 32, 48, 64, 128, 256
$pngById = @{}
foreach ($s in $sizes) {
    $bmp = Scale-To $master $s
    $bmp.Save((Join-Path $root "icon-$s.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngById[$s] = $ms.ToArray()
    $ms.Dispose(); $bmp.Dispose()
    Write-Host "wrote icon-$s.png"
}

# --- pack icon.ico (PNG entries) ---
$icoPath = Join-Path $root 'icon.ico'
$fs = [System.IO.File]::Create($icoPath)
$bwt = New-Object System.IO.BinaryWriter($fs)
$bwt.Write([uint16]0)            # reserved
$bwt.Write([uint16]1)            # type = icon
$bwt.Write([uint16]$sizes.Count) # image count
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $bytes = $pngById[$s]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bwt.Write([byte]$dim)        # width  (0 => 256)
    $bwt.Write([byte]$dim)        # height (0 => 256)
    $bwt.Write([byte]0)           # palette
    $bwt.Write([byte]0)           # reserved
    $bwt.Write([uint16]1)         # color planes
    $bwt.Write([uint16]32)        # bits per pixel
    $bwt.Write([uint32]$bytes.Length)
    $bwt.Write([uint32]$offset)
    $offset += $bytes.Length
}
foreach ($s in $sizes) { $bwt.Write([byte[]]$pngById[$s]) }
$bwt.Flush(); $bwt.Close(); $fs.Close()
Write-Host "wrote icon.ico ($($sizes.Count) sizes)"

$master.Dispose(); $cropped.Dispose(); $src.Dispose()
Write-Host "done."
