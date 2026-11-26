# Makes the running game's window borderless and the size of its monitor (borderless full screen). Waits up to
# 3 minutes for the window. The game itself is not touched otherwise.
#   .\fullscreen.ps1
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class FullWin {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public int cbSize; public RECT rcMonitor, rcWork; public uint dwFlags; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint flags);
    [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr m, ref MONITORINFO i);
    [DllImport("user32.dll", EntryPoint = "SetWindowLongPtrW")] public static extern IntPtr SetWindowLongPtr(IntPtr h, int i, IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
'@
[void][FullWin]::SetProcessDPIAware()
$p = $null
for ($i = 0; $i -lt 180 -and -not $p; $i++) {
    $p = Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
    if (-not $p) { Start-Sleep 1 }
}
if (-not $p) { 'no game window'; return }
Start-Sleep 3
$h = $p.MainWindowHandle
$m = [FullWin]::MonitorFromWindow($h, 2)
$info = New-Object FullWin+MONITORINFO
$info.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($info)
[void][FullWin]::GetMonitorInfo($m, [ref]$info)
$r = $info.rcMonitor
# WS_POPUP | WS_VISIBLE; SWP_FRAMECHANGED | SWP_SHOWWINDOW
[void][FullWin]::SetWindowLongPtr($h, -16, [IntPtr]0x90000000L)
[void][FullWin]::SetWindowPos($h, [IntPtr]::Zero, $r.Left, $r.Top, $r.Right - $r.Left, $r.Bottom - $r.Top, 0x0060)
[void][FullWin]::SetForegroundWindow($h)
"game window made borderless full screen: $($r.Right - $r.Left)x$($r.Bottom - $r.Top) at $($r.Left),$($r.Top)"
