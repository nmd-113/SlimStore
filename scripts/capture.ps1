param([Parameter(Mandatory=$true)][int]$ProcessId,[Parameter(Mandatory=$true)][string]$Destination,[switch]$ClientOnly)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class SlimStoreWindowCapture {
 [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd,out Rect rect);
 [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd,out Rect rect);
 [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd,IntPtr hdc,uint flags);
 [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr hwnd,IntPtr rect,IntPtr region,uint flags);
}
'@
$process = Get-Process -Id $ProcessId
[SlimStoreWindowCapture]::RedrawWindow($process.MainWindowHandle,[IntPtr]::Zero,[IntPtr]::Zero,0x185) | Out-Null
Start-Sleep -Milliseconds 100
$rect = New-Object SlimStoreWindowCapture+Rect
$bounds = if ($ClientOnly) {
    [SlimStoreWindowCapture]::GetClientRect($process.MainWindowHandle,[ref]$rect)
} else {
    [SlimStoreWindowCapture]::GetWindowRect($process.MainWindowHandle,[ref]$rect)
}
if (-not $bounds) { throw 'No app window found' }
$bitmap = New-Object System.Drawing.Bitmap ($rect.Right-$rect.Left),($rect.Bottom-$rect.Top)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$hdc = $graphics.GetHdc()
$flags = if ($ClientOnly) { 3 } else { 2 }
try { if (-not [SlimStoreWindowCapture]::PrintWindow($process.MainWindowHandle,$hdc,$flags)) { throw 'Window capture failed' } }
finally { $graphics.ReleaseHdc($hdc) }
try { $bitmap.Save([IO.Path]::GetFullPath($Destination),[System.Drawing.Imaging.ImageFormat]::Png) }
finally { $graphics.Dispose(); $bitmap.Dispose() }
