# Starts your own copy of the title on foxhound with the settings tested on an RTX 4070 SUPER (Series X profile,
# 2560x1440 output, DLSS Quality when NVIDIA Streamline is set up).
#
#   .\scripts\play-foxhound.ps1 -Exe 'D:\MyDump\game_scarlett_master.exe'
#   .\scripts\play-foxhound.ps1 -Exe ... -Dlss off            the title's own upscaling, no DLSS
#   .\scripts\play-foxhound.ps1 -Exe ... -Dlss balanced -FrameGen
#   .\scripts\play-foxhound.ps1 -Exe ... -SeriesS             Series S profile (1280x720 inside, faster, softer)
#   .\scripts\play-foxhound.ps1 -Exe ... -Build diag          diagnostics build (slower; captures, timelines)
#   .\scripts\play-foxhound.ps1 -Exe ... -More @{VICTORY_AMBIENT_SPECULAR='0'}   add or override switches
#
# Build first (README). Run from a normal window, not as administrator. Each run writes a log to
# %LOCALAPPDATA%\victory\logs. Every VICTORY_... switch is described in a comment where the code reads it.
param(
    [Parameter(Mandatory)][string]$Exe,
    [ValidateSet('diag', 'release')][string]$Build = 'release',
    [switch]$SeriesS,
    # 1440: output and scene 2560x1440 (Series X, the image is patched); 1080to1440: scene 1920x1080 upscaled by
    # the title; 4k: the stock Series X output (3840x2160).
    [ValidateSet('1440', '1080to1440', '4k')][string]$Res = '1440',
    # DLSS Super Resolution in place of the title's upscaling; needs NVIDIA Streamline (README).
    [ValidateSet('off', 'dlaa', 'quality', 'balanced', 'performance')][string]$Dlss = 'quality',
    # DLSS Frame Generation (RTX 40 or later, hardware-accelerated GPU scheduling on); needs DLSS.
    [switch]$FrameGen,
    # Frames begin every 1/Fps s whatever the title asks (it asks 1/30); the frame time is the layer's limit.
    [ValidateRange(1, 1000)][int]$Fps = 0,
    [hashtable]$More = @{}
)
$repo = Split-Path $PSScriptRoot -Parent
$settings = @{
    # Submission and copy machinery
    VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_RELAY = '2'; VICTORY_RELAY_COPY = '1'; VICTORY_WIDE_BACK = '1'
    VICTORY_CARD_REHASH_RUNS = '1'; VICTORY_JOIN_COPIES = '1'; VICTORY_CARD_STEPS = '2'; VICTORY_CARD_AWAKE = '1'
    VICTORY_CARD_BUFFERS_MIN_KB = '64'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_INDIRECT_BATCH = '2'
    VICTORY_UPLOAD_AHEAD = '1'; VICTORY_SHOW_AFTER = '1'; VICTORY_MAP_THREADS = '1'
    # Shader writes into big write-combined copies stay on the card (-5 ms a frame)
    VICTORY_WC_NO_BACK = '1'; VICTORY_WC_NO_BACK_MIN_KB = '1024'
    # The title's page-mapping copies kept without Windows views (no cutscene freezes at scene cuts)
    VICTORY_SOFT_VIEWS = 'copies'
    # Textures: pool with no blocks made ahead, 16 ms a pass, no catch-up, levels not on demand (sharp)
    VICTORY_TEXTURE_POOL = '1'; VICTORY_RIPE_BLOCKS = '0'; VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_TEXTURE_CATCHUP = '0'
    VICTORY_TEXTURE_STAGE_MS = '16'; VICTORY_LEVELS_ON_DEMAND = '0'
    VICTORY_PRELUDE_UPLOADS = '1'; VICTORY_SHOW_NEW_LATER = '1'; VICTORY_POOL_SMALL = '0'; VICTORY_TABLED_REREAD = '1'
    VICTORY_QUEUE_HOST_TEXTURES = '0'
    # Descriptor heaps' records
    VICTORY_HEAP_SYNC_FIRST = '1'; VICTORY_SYNC_IN_PLACE = '1'; VICTORY_SPANS_QUICK = '1'; VICTORY_SYNC_FINE = '1'
    VICTORY_HEAP_SYNC_AT_SUBMIT = '1'
    # Light without the title's ray tracing: an estimate of indirect light and of reflections
    VICTORY_AMBIENT = '0.8'; VICTORY_AMBIENT_AIR = '1.5'; VICTORY_AMBIENT_SPECULAR = '1'
    # Pixel programs keep the console's early depth test
    VICTORY_EARLY_DEPTH = '1'
    # Left out: the decal heap's defragmentation (5.6 ms of GPU a frame) and the heat-haze pass (smeared blocks)
    VICTORY_LEAVE_OUT = '02384a60fda6f2d0,731a39171b568298'
    # Pipelines the title makes are written down, so `victory warm` can make them ahead
    VICTORY_PIPELINE_BOOK = '1'
}
if (-not $SeriesS) {
    if ($Res -eq '1440') { $settings.VICTORY_NATIVE_1440P = '1' }
    elseif ($Res -eq '1080to1440') { $settings.VICTORY_RENDER_1080P = '1' }
}
if ($Dlss -ne 'off') {
    $sl = if ($env:VICTORY_STREAMLINE_DIR) { $env:VICTORY_STREAMLINE_DIR } else { 'C:\dev\sdk\streamline-sdk-v2.14.16\bin\x64' }
    if (Test-Path (Join-Path $sl 'sl.interposer.dll')) {
        $settings.VICTORY_DLSS = '1'; $settings.VICTORY_DLSS_MODE = $Dlss; $settings.VICTORY_STREAMLINE_DIR = $sl
        if ($FrameGen) { $settings.VICTORY_FRAMEGEN = '1' }
    } else {
        Write-Host "No NVIDIA Streamline in $sl (set VICTORY_STREAMLINE_DIR, see README): running without DLSS."
    }
}
if ($Fps -gt 0) { $settings.VICTORY_FRAME_INTERVAL_US = [string][int][math]::Round(1000000 / $Fps) }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
$console = if ($SeriesS) { 'series-s' } else { 'series-x' }

$victory = Join-Path $repo "target\$Build\victory.exe"
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (README)."; return }
if (-not (Test-Path -LiteralPath $Exe)) { Write-Host "No $Exe."; return }
# Switches left over in this window are cleared; path settings stay (victory.toml gives them otherwise).
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_STREAMLINE_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

$logs = Join-Path $env:LOCALAPPDATA 'victory\logs'; New-Item -ItemType Directory -Force $logs | Out-Null
$log = Join-Path $logs ('foxhound-{0:yyyyMMdd-HHmmss}.log' -f (Get-Date))
$writer = [IO.StreamWriter]::new($log, $false, [Text.UTF8Encoding]::new($false)); $writer.AutoFlush = $true
$writer.WriteLine("# console=$console build=$Build switches: " + (($settings.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' '))
Write-Host "Log: $log"
Push-Location $repo   # victory.toml is read from here
try { & $victory run $Exe --console $console --execute 2>&1 | ForEach-Object { $line = "$_"; $writer.WriteLine($line); $line } }
finally { Pop-Location; $writer.Dispose() }
