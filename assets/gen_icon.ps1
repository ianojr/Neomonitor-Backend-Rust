# Generates assets/neomonitor.ico (multi-resolution) and assets/neomonitor.png
# Design: neon activity waveform on a dark rounded tile, matching the TUI cyan accent
# with a magenta flourish (GPU series). Reproducible asset generator.
Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$icoPath = Join-Path $here 'neomonitor.ico'
$pngPath = Join-Path $here 'neomonitor.png'

function New-RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function New-IconBitmap([int]$size) {
    $s = $size / 256.0
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.Clear([System.Drawing.Color]::Transparent)
    # Work in a fixed 256-unit space; the transform maps it to the target size.
    $g.ScaleTransform($s, $s)

    $cyan = [System.Drawing.Color]::FromArgb(255, 34, 211, 238)
    $magenta = [System.Drawing.Color]::FromArgb(255, 232, 121, 249)
    $glowCyan = [System.Drawing.Color]::FromArgb(60, 34, 211, 238)
    $glowMagenta = [System.Drawing.Color]::FromArgb(60, 232, 121, 249)

    $pad = 10.0
    $tile = New-RoundedPath $pad $pad (256.0 - 2 * $pad) (256.0 - 2 * $pad) 58.0

    # Background: vertical dark-navy gradient.
    $bgRect = New-Object System.Drawing.RectangleF 0, 0, 256, 256
    $bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush $bgRect, ([System.Drawing.Color]::FromArgb(255, 14, 22, 40)), ([System.Drawing.Color]::FromArgb(255, 4, 6, 13)), 90.0
    $g.FillPath($bg, $tile)

    # Faint scope grid, only where it will read.
    if ($size -ge 128) {
        $gridPen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(18, 34, 211, 238)), 1.4
        $g.SetClip($tile)
        foreach ($f in @(0.25, 0.5, 0.75)) {
            $v = $pad + (256.0 - 2 * $pad) * $f
            $g.DrawLine($gridPen, $v, $pad, $v, 256.0 - $pad)
            $g.DrawLine($gridPen, $pad, $v, 256.0 - $pad, $v)
        }
        $g.ResetClip()
        $gridPen.Dispose()
    }

    # Border for definition.
    $border = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(90, 34, 211, 238)), 4.0
    $g.DrawPath($border, $tile)
    $border.Dispose()

    # Activity waveform (ECG-like pulse): flat, small dip, sharp peak, trough, recover, flat.
    $pts = @(
        @(26.0, 150.0), @(88.0, 150.0), @(102.0, 166.0), @(116.0, 150.0),
        @(142.0, 48.0), @(164.0, 206.0), @(182.0, 150.0), @(238.0, 150.0)
    )
    $pf = New-Object 'System.Drawing.PointF[]' $pts.Count
    for ($i = 0; $i -lt $pts.Count; $i++) {
        $pf[$i] = New-Object System.Drawing.PointF ([single]$pts[$i][0]), ([single]$pts[$i][1])
    }
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddLines($pf)

    $wb = $path.GetBounds()
    $lw = [Math]::Max($wb.Width, 1.0)

    $glowBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $wb, $glowCyan, $glowMagenta, 0.0
    $glowPen = New-Object System.Drawing.Pen $glowBrush, 22.0
    $glowPen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $glowPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $glowPen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawPath($glowPen, $path)

    $mainBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $wb, $cyan, $magenta, 0.0
    $mainPen = New-Object System.Drawing.Pen $mainBrush, 15.0
    $mainPen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $mainPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $mainPen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawPath($mainPen, $path)

    if ($size -ge 32) {
        $corePen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(230, 224, 255, 255)), 5.0
        $corePen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
        $g.DrawPath($corePen, $path)
        $corePen.Dispose()
    }

    # Bright pulse node at the peak.
    $dotBrush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 236, 254, 255))
    $g.FillEllipse($dotBrush, 142.0 - 8.0, 48.0 - 8.0, 16.0, 16.0)
    $dotBrush.Dispose()

    $glowPen.Dispose(); $glowBrush.Dispose()
    $mainPen.Dispose(); $mainBrush.Dispose()
    $path.Dispose(); $bg.Dispose(); $tile.Dispose()
    $g.Dispose()
    return $bmp
}

function Get-PngBytes([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Dispose()
    return , $bytes
}

$sizes = @(16, 24, 32, 48, 64, 128, 256)
$images = @()
foreach ($sz in $sizes) {
    $bmp = New-IconBitmap $sz
    $images += , (Get-PngBytes $bmp)
    if ($sz -eq 256) { $bmp.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png) }
    $bmp.Dispose()
}

# Assemble an ICO container holding PNG payloads.
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $sz = $sizes[$i]
    $w = if ($sz -ge 256) { 0 } else { $sz }
    $data = $images[$i]
    $bw.Write([Byte]$w); $bw.Write([Byte]$w); $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$data.Length); $bw.Write([UInt32]$offset)
    $offset += $data.Length
}
foreach ($data in $images) { $bw.Write($data) }
$bw.Flush()
[System.IO.File]::WriteAllBytes($icoPath, $ms.ToArray())
$bw.Dispose(); $ms.Dispose()

Write-Output ("wrote {0} ({1} bytes) and {2}" -f $icoPath, (Get-Item $icoPath).Length, $pngPath)
