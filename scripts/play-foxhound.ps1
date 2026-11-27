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
# In the game, F1 opens a settings menu (arrow keys pick and change, Enter does it). What it saves goes to
# victory-settings.txt in the repository's folder and is applied here at every start, over the options above;
# delete that file to go back to them.
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
    # Frames shown for each one rendered with -FrameGen: 2 to 6 (more than 2 needs an RTX 50; the layer asks the
    # driver how many this card does and gives no more).
    [ValidateRange(2, 6)][int]$FrameGenX = 2,
    # Frame generation is made ready at every start with Streamline, on or off, so that the F1 menu switches it
    # and changes its amount while playing. -NoFrameGenReady: only with -FrameGen, as before.
    [switch]$NoFrameGenReady,
    # Which frame generation: auto (DLSS Frame Generation where it runs, else AMD's FSR 3), dlss, or fsr (FSR 3
    # even on an RTX 40/50; 2X only). FSR 3 needs AMD's FidelityFX DLLs (VICTORY_FIDELITYFX_DIR, else
    # amd-sdk\FidelityFX\bin here, where Get-AMD-FSR.cmd puts them) and, on Series X, DLSS for the upscaling (the title's own takes Series S sizes).
    [ValidateSet('auto', 'dlss', 'fsr')][string]$FgType = 'auto',
    # Frames begin every 1/Fps s whatever the title asks (it asks 1/30). 1000, the default: no cap, a frame
    # begins as soon as the last is done. 30: the title's own rate.
    [ValidateRange(1, 1000)][int]$Fps = 1000,
    # Every shader and the loading pipelines made first (`victory preload`), then the game; the F1 menu's
    # PRELOAD SHADERS starts this again with it.
    [switch]$Preload,
    [hashtable]$More = @{}
)
$repo = Split-Path $PSScriptRoot -Parent
# The F1 menu's "PRELOAD SHADERS" restarts this with -Preload (and VICTORY_PRELOAD=1, read here before the
# switches below are cleared).
$preload = $Preload -or $env:VICTORY_PRELOAD -eq '1'
# What the F1 menu saved: its own choices (VICTORY_MENU_...) stand in for this script's options, and its
# switches go over the settings below. An empty value unsets a switch.
$menuFile = Join-Path $repo 'victory-settings.txt'
$menu = [ordered]@{}
if (Test-Path -LiteralPath $menuFile) {
    foreach ($line in Get-Content -LiteralPath $menuFile) {
        if ($line -match '^\s*(VICTORY_[A-Z0-9_]+)\s*=(.*)$') { $menu[$Matches[1]] = $Matches[2].Trim() }
    }
}
$fpsWanted = $Fps
if ($menu.Contains('VICTORY_MENU_CONSOLE')) { $SeriesS = $menu['VICTORY_MENU_CONSOLE'] -eq 'series-s' }
if ($menu['VICTORY_MENU_RES'] -in '1440', '1080to1440', '4k') { $Res = $menu['VICTORY_MENU_RES'] }
if ($menu['VICTORY_MENU_DLSS'] -in 'off', 'dlaa', 'quality', 'balanced', 'performance') { $Dlss = $menu['VICTORY_MENU_DLSS'] }
$timesWanted = $FrameGenX
if ($menu.Contains('VICTORY_MENU_FRAMEGEN')) {
    # 0: off; 2 to 6: that many frames shown for each one rendered (1, from an older file: 2).
    $FrameGen = $menu['VICTORY_MENU_FRAMEGEN'] -match '^[1-6]$'
    if ($menu['VICTORY_MENU_FRAMEGEN'] -match '^[2-6]$') { $timesWanted = [int]$menu['VICTORY_MENU_FRAMEGEN'] }
}
if ($menu['VICTORY_MENU_FG_TYPE'] -in 'auto', 'dlss', 'fsr') { $FgType = $menu['VICTORY_MENU_FG_TYPE'] }
if ($menu['VICTORY_MENU_FPS'] -match '^\d+$' -and [int]$menu['VICTORY_MENU_FPS'] -ge 1 -and [int]$menu['VICTORY_MENU_FPS'] -le 1000) { $fpsWanted = [int]$menu['VICTORY_MENU_FPS'] }
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
    # NVIDIA's files: where VICTORY_STREAMLINE_DIR says, else where Get-NVIDIA-DLSS.cmd puts them.
    $sl = if ($env:VICTORY_STREAMLINE_DIR) { $env:VICTORY_STREAMLINE_DIR } else { Join-Path $repo 'nvidia-sdk\Streamline\bin\x64' }
    if (Test-Path (Join-Path $sl 'sl.interposer.dll')) {
        $settings.VICTORY_DLSS = '1'; $settings.VICTORY_DLSS_MODE = $Dlss; $settings.VICTORY_STREAMLINE_DIR = $sl
        if ($FrameGen) { $settings.VICTORY_FRAMEGEN = '1'; $settings.VICTORY_FRAMEGEN_X = [string]$timesWanted }
        elseif (-not $NoFrameGenReady) { $settings.VICTORY_FRAMEGEN = '1'; $settings.VICTORY_FRAMEGEN_X = '0' }   # ready, off
        # Frame generation takes 1.5 to 2 GB of the card's memory, which the layer otherwise fills with textures up
        # to nine tenths of its budget (on a 12 GB card it then ran out: no generated frames). On from the start, it
        # is left a fifth of the budget; ready but off, a little less is held back.
        if ($settings.VICTORY_FRAMEGEN -eq '1') { $settings.VICTORY_TEXTURE_ROOM_PERCENT = if ($FrameGen) { '80' } else { '86' } }
    } else {
        Write-Host "No NVIDIA files in ${sl}: running without DLSS and frame generation. Get-NVIDIA-DLSS.cmd in the"
        Write-Host "repository folder fetches them from NVIDIA once (about 70 MB); VICTORY_STREAMLINE_DIR names another folder."
    }
}
# AMD FSR 3 frame generation: made ready as DLSS Frame Generation is (on, or ready and off for the F1 menu).
# auto: the layer uses it only where DLSS Frame Generation does not run; fsr: DLSS Frame Generation is not asked.
if ($FgType -ne 'dlss' -and ($FrameGen -or -not $NoFrameGenReady)) {
    $fx = if ($env:VICTORY_FIDELITYFX_DIR) { $env:VICTORY_FIDELITYFX_DIR } else { Join-Path $repo 'amd-sdk\FidelityFX\bin' }
    # Asked whatever is here: the layer checks the files and the card, and the start's notice (on the picture for
    # its first 30 s, and in this window) says what runs and why not. On Series X without DLSS it stays off: the
    # title's own upscaling, which it needs, takes the Series S sizes only.
    $settings.VICTORY_FSR_FRAMEGEN = '1'; $settings.VICTORY_FIDELITYFX_DIR = $fx
    if (-not (Test-Path (Join-Path $fx 'amd_fidelityfx_framegeneration_dx12.dll'))) {
        Write-Host "No AMD FSR 3 files in ${fx}: Get-AMD-FSR.cmd in the repository's folder fetches them from AMD (about"
        Write-Host '30 MB, once). Needed only on cards without DLSS frame generation, or with FSR 3 chosen in F1.'
    }
    if ($FgType -eq 'fsr') { $settings.Remove('VICTORY_FRAMEGEN') }
    if (-not $settings.VICTORY_FRAMEGEN_X) { $settings.VICTORY_FRAMEGEN_X = if ($FrameGen) { [string]$timesWanted } else { '0' } }
    if (-not $settings.VICTORY_TEXTURE_ROOM_PERCENT) { $settings.VICTORY_TEXTURE_ROOM_PERCENT = if ($FrameGen) { '80' } else { '86' } }
}
if ($fpsWanted -gt 0) { $settings.VICTORY_FRAME_INTERVAL_US = [string][int][math]::Round(1000000 / $fpsWanted) }
foreach ($k in $menu.Keys) {
    if ($k -like 'VICTORY_MENU_*') { continue }
    if ($menu[$k]) { $settings[$k] = $menu[$k] } else { $settings.Remove($k) }
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
$console = if ($SeriesS) { 'series-s' } else { 'series-x' }
# For the menu: what it shows as the current choices, where it saves, and how it starts the game again
# (-More is not carried over a restart from the menu).
$settings.VICTORY_MENU_CONSOLE = $console
$settings.VICTORY_MENU_RES = $Res
$settings.VICTORY_MENU_DLSS = if ($settings.VICTORY_DLSS -eq '1') { $Dlss } else { 'off' }
$settings.VICTORY_MENU_FRAMEGEN = if ($settings.VICTORY_FRAMEGEN -eq '1' -or $settings.VICTORY_FSR_FRAMEGEN -eq '1') { [string]$settings.VICTORY_FRAMEGEN_X } else { '0' }
$settings.VICTORY_MENU_FG_TYPE = $FgType
$settings.VICTORY_MENU_FPS = [string]$fpsWanted
$settings.VICTORY_MENU_FILE = $menuFile
$settings.VICTORY_MENU_PLAY = $MyInvocation.MyCommand.Path
$settings.VICTORY_MENU_ARGS = "-Exe '$($Exe -replace "'", "''")' -Build $Build$(if ($NoFrameGenReady) { ' -NoFrameGenReady' })"

$victory = Join-Path $repo "target\$Build\victory.exe"
if (-not (Test-Path $victory)) {
    # Not built yet: built here, once.
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Write-Host "No $victory, and no cargo to build it with: install Rust (README), then start this again."; return }
    Write-Host 'Building the layer (a few minutes the first time) ...'
    Push-Location $repo
    try {
        if ($Build -eq 'diag') { cargo build --locked --profile diag -p victory --features diagnostics }
        else { cargo build --release --locked -p victory }
    } finally { Pop-Location }
    if (-not (Test-Path $victory)) { Write-Host "The build did not make $victory."; return }
}
if (-not (Test-Path -LiteralPath $Exe)) { Write-Host "No $Exe."; return }
# Switches left over in this window are cleared; path settings stay (victory.toml gives them otherwise).
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_STREAMLINE_DIR', 'VICTORY_FIDELITYFX_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

$logs = Join-Path $env:LOCALAPPDATA 'victory\logs'; New-Item -ItemType Directory -Force $logs | Out-Null
$log = Join-Path $logs ('victory-nvidia-{0:yyyyMMdd-HHmmss}.log' -f (Get-Date))
$writer = [IO.StreamWriter]::new($log, $false, [Text.UTF8Encoding]::new($false)); $writer.AutoFlush = $true
$writer.WriteLine("# console=$console build=$Build preload=$preload switches: " + (($settings.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' '))
Write-Host "Log: $log"
Push-Location $repo   # victory.toml is read from here
try {
    if ($preload) {
        # With this start's switches (the translations depend on them); its lines go to the log too.
        Write-Host 'Preloading shaders before the game starts. This takes a while (see the lines below).'
        $began = Get-Date
        # (Split-Path -LiteralPath with -Parent: no such parameter set in Windows PowerShell 5.1.)
        & $victory preload ([IO.Path]::GetDirectoryName($Exe)) 2>&1 | ForEach-Object { $line = "$_"; $writer.WriteLine("# preload: $line"); $line }
        $how = if ($LASTEXITCODE -eq 0) { 'done' } else { "ended with code $LASTEXITCODE" }
        $said = 'Preload {0} after {1:N1} min. Starting the game.' -f $how, ((Get-Date) - $began).TotalMinutes
        $writer.WriteLine("# $said"); Write-Host $said
    }
}
finally { Pop-Location }
Push-Location $repo
try { & $victory run $Exe --console $console --execute 2>&1 | ForEach-Object { $line = "$_"; $writer.WriteLine($line); $line } }
finally { Pop-Location; $writer.Dispose() }
