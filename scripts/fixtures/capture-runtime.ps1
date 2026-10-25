param([Parameter(Mandatory)][string] $ArtifactDirectory)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class RuntimeWindowCapture {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr device, uint flags);
}
'@

$ownedProcesses = Get-CimInstance Win32_Process | Where-Object {
    ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($ArtifactDirectory + '\', [StringComparison]::OrdinalIgnoreCase)) -or
    ($_.CommandLine -and $_.CommandLine.Contains($ArtifactDirectory))
}
$ownedProcesses | Select-Object Name, ProcessId, ParentProcessId, ExecutablePath, CommandLine |
    ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $ArtifactDirectory 'failure-processes.json')
foreach ($owned in $ownedProcesses | Where-Object { $_.Name -eq 'rebellion2-launcher.exe' }) {
    $process = Get-Process -Id $owned.ProcessId -ErrorAction SilentlyContinue
    if (!$process -or $process.MainWindowHandle -eq [IntPtr]::Zero) { continue }
    $rect = New-Object RuntimeWindowCapture+Rect
    if (![RuntimeWindowCapture]::GetWindowRect($process.MainWindowHandle, [ref]$rect)) { continue }
    $bitmap = New-Object System.Drawing.Bitmap(($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top))
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $device = $graphics.GetHdc()
    try { [RuntimeWindowCapture]::PrintWindow($process.MainWindowHandle, $device, 2) | Out-Null }
    finally { $graphics.ReleaseHdc($device) }
    $bitmap.Save((Join-Path $ArtifactDirectory "failure-$($owned.ProcessId).png"))
    $graphics.Dispose()
    $bitmap.Dispose()
}
