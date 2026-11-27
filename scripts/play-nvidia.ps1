# Starts the title on an NVIDIA card: Series S, the game's own 720p picture taken to 1080p by DLSS, the F1
# settings menu in the game (measured on an RTX 5070: about 15-17 fps before generated frames).
#   scripts\play-nvidia.ps1 -Exe <path of your own copy's executable>
# DLSS and DLSS frame generation need a few NVIDIA files (about 70 MB) that this repository does not carry:
# run Get-NVIDIA-DLSS.cmd in the repository's folder once, and they are fetched from NVIDIA's GitHub into
# nvidia-sdk\. Without them the game runs with its picture stretched to the screen instead.
# The layer is built here if it is not built yet, and built again with DLSS once those files are there.
# In the game: F1 = settings (arrows pick and change, Enter saves; kept in victory-settings.txt next to this
# folder), F3 = the numbers panel.
# -NoSmoothMotion  run target\<build>\victory.exe itself. By default a copy named victory_sm.exe is started instead,
#                  so a Smooth Motion profile set for that exe in the NVIDIA App applies every time.
# -NoDlss          no DLSS: the picture is stretched (fast, softer)
# -SeriesX         the Series X profile instead (much slower; no DLSS there)
# -Quality         fewer stretched/jumping bodies, slower (relay stage 1, no card steps)
# -RayTracing      the title's real ray tracing (the structure builder runs). Shaders build for minutes the
#                  first time, and DLSS frame generation is left off with it (the two together do not work yet).
# -Resolution      the older path: the game renders at this height (2160, 1440, 1080, 720, 540 or 480) and the
#                  title's own upscaler does the rest, without DLSS. The play-S-*.ps1 and play-X-*.ps1 scripts use it.
# -Adapter         on a PC with more than one card, the one to use: part of its name (-Adapter 5060) or its index
#                  (-Adapter '#1'). Default: Windows' default card.
# -Build           target\diag (the default, the build all measurements are of) or target\release
# -DlssDir         the folder with nvngx_dlss.dll, if not nvidia-sdk\DLSS\lib\Windows_x86_64\rel
# -StreamlineDir   the folder with sl.interposer.dll (frame generation), if not nvidia-sdk\Streamline\bin\x64
# -Log             the layer's log goes to a file in %LOCALAPPDATA%\VIctory\logs instead of this window
# -Capture         F9 in the game saves that frame and the pictures it drew, in %LOCALAPPDATA%\VIctory\captures
# -WindowsBarMB    maximum MiB of title memory windows in CPU-mapped VRAM (by default 3072 on PCs with under
#                  24 GB of RAM, else 0; -WindowsBarMB 0 turns it off).
# -More            other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$NoSmoothMotion,
    [switch]$NoDlss,
    [switch]$SeriesX,
    [switch]$Quality,
    [switch]$RayTracing,
    [ValidateSet('2160', '1440', '1080', '720', '540', '480')][string]$Resolution,
    [string]$Adapter,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [string]$DlssDir,
    [string]$StreamlineDir,
    [switch]$Log,
    [switch]$Capture,
    [hashtable]$More = @{},
    [uint32]$WindowsBarMB
)
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; return }
$Exe = (Resolve-Path $Exe).Path

# NVIDIA's own files: where they are (Get-NVIDIA-DLSS.cmd puts them in nvidia-sdk\). Nothing is downloaded here.
if (-not $DlssDir) { $DlssDir = $env:VICTORY_DLSS_DIR }
if (-not $StreamlineDir) { $StreamlineDir = $env:VICTORY_STREAMLINE_DIR }
$sdkDlss = Join-Path $root 'nvidia-sdk\DLSS'
$sdkDlssLib = Join-Path $sdkDlss 'lib\Windows_x86_64\x64\nvsdk_ngx_d.lib'
$sdkStreamline = Join-Path $root 'nvidia-sdk\Streamline\bin\x64'
$wantDlss = -not $NoDlss -and -not $SeriesX -and -not $Resolution
if (-not $DlssDir -and (Test-Path (Join-Path $sdkDlss 'lib\Windows_x86_64\rel\nvngx_dlss.dll'))) { $DlssDir = Join-Path $sdkDlss 'lib\Windows_x86_64\rel' }
if (-not $StreamlineDir -and (Test-Path (Join-Path $sdkStreamline 'sl.interposer.dll'))) { $StreamlineDir = $sdkStreamline }
if ($wantDlss -and -not $DlssDir) {
    Write-Host 'No DLSS files here: the picture is stretched to the screen instead. Get-NVIDIA-DLSS.cmd (in the'
    Write-Host "repository's folder) fetches them from NVIDIA once, about 70 MB, with frame generation's."
}

# The build: made here if there is none, and made again with DLSS once its SDK is there (the diag build only).
$victory = Join-Path $root "target\$Build\victory.exe"
$built = Join-Path $root "target\$Build\built-with.txt"
$hasDlss = (Test-Path $built) -and ((Get-Content $built -Raw) -match 'dlss')
$canDlss = $wantDlss -and (Test-Path $sdkDlssLib)
if ($Build -eq 'diag' -and (-not (Test-Path $victory) -or ($canDlss -and -not $hasDlss -and (Test-Path $built)))) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Write-Host 'No build and no cargo: install Rust (README.md), then start this again.'; return }
    Write-Host 'Building the layer (a few minutes the first time) ...'
    & (Join-Path $PSScriptRoot 'build-nvidia.ps1')
    $hasDlss = (Test-Path $built) -and ((Get-Content $built -Raw) -match 'dlss')
}
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (scripts\build-nvidia.ps1)."; return }
# A build nobody noted the making of (cargo by hand) is taken at its word: with DLSS if the files are there.
$dlss = $wantDlss -and $DlssDir -and ($hasDlss -or -not (Test-Path $built))

$settings = @{
    # The play set. Textures are waited for: one loaded once at the start could stay empty for the run otherwise.
    VICTORY_TEXTURE_NO_WAIT = '0'; VICTORY_RELAY = '2'; VICTORY_CARD_STEPS = '2'
    VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'
    # Copy-backs caught half-written are fetched again (torn.rs).
    VICTORY_FETCH_CHECK = '1'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    # The renderer's light stand-in is off (its coarse estimate made indoor light blotchy).
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0'; VICTORY_D3D12_DRED = 'light'
    VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_UPLOAD_AHEAD = '1'
    # Fog bounded by 1.5 times the light around; a NaN a shader writes is made 0 before blur and the upscaler spread it.
    VICTORY_AMBIENT_AIR = '1.5'; VICTORY_NAN_SCRUB = '1'
    # Wait for new shaders rather than skip their draws (each pipeline is waited for at most once). The F1 menu's
    # SHADER WAIT MS changes it: 0 never waits, things then show up as their shaders are done.
    VICTORY_PIPELINE_WAIT_MS = '10000'
    # NVIDIA caps the shader-visible view heap at 1,000,000: without more dynamic slots thousands of draws a frame
    # are dropped (descriptor/host.rs dynamic_slots).
    VICTORY_DYNAMIC_SLOTS = '524288'
    # Memory compares on the card instead of the CPU.
    VICTORY_CARD_BUFFERS_MIN_MB = '1'; VICTORY_CARD_BUFFERS_MB = '1536'
}
$console = if ($SeriesX) { 'series-x' } else { 'series-s' }
if ($Resolution) {
    # The older path: a render scale and the title's own upscaler (rendered height over output height).
    $outHeight = if ($SeriesX) { 2160 } else { 1080 }
    if ([int]$Resolution -gt $outHeight) { Write-Host "That console outputs ${outHeight}p: ${Resolution}p is above it."; return }
    $settings.VICTORY_RENDER_SCALE = ([int]$Resolution / $outHeight).ToString('0.####', [Globalization.CultureInfo]::InvariantCulture)
    $settings.VICTORY_OWN_UPSCALER = '1'
} elseif ($dlss) {
    $settings.VICTORY_UPSCALE = 'dlss'; $settings.VICTORY_DLSS_DIR = $DlssDir
} elseif (-not $SeriesX) {
    $settings.VICTORY_UPSCALE = 'stretch'
}
if ($StreamlineDir) { $settings.VICTORY_STREAMLINE_DIR = $StreamlineDir }
if ($Quality) { $settings.VICTORY_RELAY = '1'; $settings.VICTORY_CARD_STEPS = '0' }
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
if ($Adapter) { $settings.VICTORY_ADAPTER = $Adapter }
if ($PSBoundParameters.ContainsKey('WindowsBarMB')) {
    $settings.VICTORY_WINDOWS_BAR_MB = $WindowsBarMB.ToString([Globalization.CultureInfo]::InvariantCulture)
} else {
    # Without -WindowsBarMB, PCs with under 24 GB of RAM get 3072 (PR #6). Windows lets a card share at most half
    # the RAM, and the windows filled that on a 16 GB PC (RTX 3060) about a minute into the world (0x8007000E, then
    # a driver fault). 32 GB and more never got there, so they keep the runtime default (0, off).
    $ram = try { (Get-CimInstance Win32_ComputerSystem -ErrorAction Stop).TotalPhysicalMemory } catch { 0 }
    if ($ram -gt 0 -and $ram -lt 24GB) {
        $settings.VICTORY_WINDOWS_BAR_MB = '3072'
        Write-Host "$([math]::Round($ram / 1GB)) GB of RAM: title memory windows in the card's memory the CPU maps, up to 3 GB (-WindowsBarMB 0 turns this off)"
    }
}

# What the F1 menu saved (victory-settings.txt, git-ignored), over the settings above. An empty value unsets one.
$menuFile = Join-Path $root 'victory-settings.txt'
$smooth = -not $NoSmoothMotion
if (Test-Path $menuFile) {
    foreach ($line in Get-Content $menuFile) {
        if ($line -notmatch '^\s*(VICTORY_[A-Z0-9_]+)\s*=(.*)$') { continue }
        $k = $Matches[1]; $v = $Matches[2].Trim()
        if ($k -eq 'VICTORY_MENU_SMOOTH_MOTION') { $smooth = $v -ne '0'; continue }
        if ($k -eq 'VICTORY_MENU_CONSOLE') { if ($v) { $console = $v }; continue }
        if ($v) { $settings[$k] = $v } else { $settings.Remove($k) }
    }
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
# DLSS only where it can be: a menu choice of it without the files or the build falls back to the stretch.
if ($settings.VICTORY_UPSCALE -eq 'dlss' -and -not $dlss) { $settings.VICTORY_UPSCALE = 'stretch' }
# DLSS frame generation and real ray tracing do not work together yet (0.1 frames a second): with ray tracing
# on, frame generation is left off and Smooth Motion can do the extra frames.
if ($settings.VICTORY_BVH_BUILDER -eq '1' -and $settings.VICTORY_FRAME_GEN -and $settings.VICTORY_FG_WITH_RT -ne '1') {
    Write-Host 'Ray tracing is on: DLSS frame generation is left off (the two together do not work yet).'
    $settings.Remove('VICTORY_FRAME_GEN')
}
if ($settings.VICTORY_FRAME_GEN -and -not $StreamlineDir) {
    Write-Host 'Frame generation needs the Streamline files (Get-NVIDIA-DLSS.cmd): left off.'
    $settings.Remove('VICTORY_FRAME_GEN')
}
# Smooth Motion is a second frame generator: off while DLSS frame generation is on.
if ($settings.VICTORY_FRAME_GEN -and $settings.VICTORY_FRAME_GEN -ne '1') { $smooth = $false }
# For the menu: what it shows as the current values, where it saves, and how it starts the game again.
$settings.VICTORY_MENU_SMOOTH_MOTION = if ($smooth) { '1' } else { '0' }
$settings.VICTORY_MENU_CONSOLE = $console
$settings.VICTORY_MENU_FILE = $menuFile
$settings.VICTORY_MENU_PLAY = $MyInvocation.MyCommand.Path
$again = @("-Exe '$($Exe -replace "'", "''")'", "-Build $Build")
foreach ($s in 'NoSmoothMotion', 'NoDlss', 'SeriesX', 'Quality', 'RayTracing', 'Log', 'Capture') {
    if ($PSBoundParameters[$s]) { $again += "-$s" }
}
foreach ($s in 'Resolution', 'Adapter', 'DlssDir', 'StreamlineDir') {
    if ($PSBoundParameters[$s]) { $again += "-$s '$($PSBoundParameters[$s] -replace "'", "''")'" }
}
if ($PSBoundParameters.ContainsKey('WindowsBarMB')) { $again += "-WindowsBarMB $WindowsBarMB" }
$settings.VICTORY_MENU_ARGS = $again -join ' '

$picture = if ($settings.VICTORY_RENDER_SCALE) { "render scale $($settings.VICTORY_RENDER_SCALE), the title's upscaler" }
    elseif ($settings.VICTORY_UPSCALE -eq 'dlss') { 'DLSS' }
    elseif ($settings.VICTORY_UPSCALE -eq 'stretch') { 'stretched (no DLSS)' }
    else { "the title's upscaler" }
$extras = ''
if ($settings.VICTORY_FRAME_GEN) { $extras += ", frame generation $($settings.VICTORY_FRAME_GEN)x" }
if ($settings.VICTORY_BVH_BUILDER -eq '1') { $extras += ', ray tracing' }
Write-Host "Series $(if ($console -eq 'series-x') { 'X' } else { 'S' }), $picture$extras. F1 in the game: settings."

if ($smooth) {
    # A copy under the name the NVIDIA App's Smooth Motion profile is set for, refreshed when the build changes.
    $sm = Join-Path (Split-Path $victory) 'victory_sm.exe'
    if (-not (Test-Path $sm) -or (Get-Item $sm).LastWriteTime -lt (Get-Item $victory).LastWriteTime) {
        try { Copy-Item $victory $sm -Force -ErrorAction Stop }
        catch {
            # Still held by a game that has not quite gone: Windows lets it be renamed out of the way.
            Rename-Item $sm "victory_sm.exe.old-$(Get-Date -Format HHmmss)" -ErrorAction SilentlyContinue
            Copy-Item $victory $sm -Force
        }
    }
    $victory = $sm
}
Get-ChildItem (Join-Path (Split-Path $victory) '*.old-*') -ErrorAction SilentlyContinue | Remove-Item -ErrorAction SilentlyContinue

# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
# A shader cache of its own: one an earlier version of this fork filled does not fit this renderer's pipelines.
$toml = Join-Path $root 'victory.toml'
if (-not $env:VICTORY_CACHE_DIR -and -not ((Test-Path $toml) -and (Select-String -Path $toml -Pattern '^\s*cache_dir\s*=' -Quiet))) {
    $env:VICTORY_CACHE_DIR = Join-Path $env:LOCALAPPDATA 'VIctory\cache-nvidia-2'
}
$stamp = Get-Date -Format 'MMdd-HHmmss'
if ($Capture) {
    $env:VICTORY_CAPTURE = Join-Path $env:LOCALAPPDATA "VIctory\captures\$stamp"
    $env:VICTORY_DUMP_TARGETS = '1'
    New-Item -ItemType Directory -Force $env:VICTORY_CAPTURE | Out-Null
    Write-Host "F9 saves a frame to $env:VICTORY_CAPTURE"
}
$runArgs = @('run', "`"$Exe`"", '--execute', '--console', $console)
Push-Location $root   # victory.toml is looked for here
try {
    if ($Log) {
        $logs = Join-Path $env:LOCALAPPDATA 'VIctory\logs'
        New-Item -ItemType Directory -Force $logs | Out-Null
        Get-ChildItem $logs -Filter 'play-*.log' | Sort-Object LastWriteTime -Descending | Select-Object -Skip 4 | Remove-Item -ErrorAction SilentlyContinue
        Write-Host "log: $logs\play-$stamp.log"
        Start-Process -FilePath $victory -ArgumentList $runArgs -NoNewWindow -Wait -RedirectStandardError "$logs\play-$stamp.log"
    } else {
        Start-Process -FilePath $victory -ArgumentList $runArgs -NoNewWindow -Wait
    }
} finally { Pop-Location }
