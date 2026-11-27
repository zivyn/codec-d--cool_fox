# One-click the title on trustno1-coder/VIctory-NVIDIA (foxhound base): DLSS Super Resolution + DLSS frame generation.
# PLAY_FRAMEGEN.cmd runs this. What it does, in order:
#   1. finds the game folder (game_dir.txt in the repository folder, else a search of every drive)
#   2. backs up the saves (%LOCALAPPDATA%\victory\data\7952987A) and checks every file by SHA-256
#   3. gets NVIDIA's DLSS files once (scripts\get-nvidia-sdks.ps1, from github.com/NVIDIA-RTX/Streamline)
#   4. starts scripts\play-foxhound.ps1, which builds the layer the first time (a few minutes)
# Modes: 1 Series X 1440p + DLSS Quality + frame generation 2x (default)
#        2 same without frame generation
#        3 Series S + DLSS Quality + frame generation 2x (faster, softer)
#        4 Series X 1440p + DLSS Performance + frame generation 2x
param(
    [ValidateSet('1', '2', '3', '4', '5', '6')][string]$Mode = '1',
    [string]$GameDir = '',
    [switch]$Check,
    [switch]$NoBackup
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent

# A game folder holds the executable and its data (a lone copy of the exe does not count).
function Test-GameDir([string]$d) {
    if (-not $d -or -not (Test-Path (Join-Path $d 'game_scarlett_master.exe'))) { return $false }
    return (Test-Path (Join-Path $d 'shaders.rpf')) -or (Test-Path (Join-Path $d 'MicrosoftGame.config'))
}
function Find-GameDir {
    $txt = Join-Path $repo 'game_dir.txt'
    if (Test-Path $txt) {
        $d = (Get-Content $txt -Encoding UTF8 -TotalCount 1).Trim().Trim('"')
        if (Test-GameDir $d) { return $d }
    }
    foreach ($drive in (Get-PSDrive -PSProvider FileSystem | Where-Object { $null -ne $_.Free })) {
        $cands = @(Get-ChildItem $drive.Root -Directory -ErrorAction SilentlyContinue)
        $cands += @($cands | ForEach-Object { Get-ChildItem $_.FullName -Directory -ErrorAction SilentlyContinue })
        foreach ($c in $cands) { if (Test-GameDir $c.FullName) { return $c.FullName } }
    }
    return $null
}

if (-not $GameDir) { $GameDir = Find-GameDir }
if (-not (Test-GameDir $GameDir)) {
    Write-Host 'Game folder not found. Put its full path on the first line of game_dir.txt in this folder.'
    exit 1
}
$gameExe = Join-Path $GameDir 'game_scarlett_master.exe'

switch ($Mode) {
    '1' { $opts = @{ FrameGen = $true }; $what = 'Series X 2560x1440, DLSS Quality, frame generation 2x' }
    '2' { $opts = @{}; $what = 'Series X 2560x1440, DLSS Quality, no frame generation' }
    '3' { $opts = @{ SeriesS = $true; FrameGen = $true }; $what = 'Series S, DLSS Quality, frame generation 2x' }
    '4' { $opts = @{ Dlss = 'performance'; FrameGen = $true }; $what = 'Series X 2560x1440, DLSS Performance, frame generation 2x' }
    '5' { $opts = @{ SeriesS = $true; Dlss = 'performance'; FrameGen = $true; Fps = 1000 }; $what = 'EXTREME: Series S, DLSS Performance, frame generation 2x, no cap, big pools, High priority' }
    '6' { $opts = @{ Dlss = 'performance'; FrameGen = $true; Fps = 1000 }; $what = 'EXTREME: Series X 2560x1440, DLSS Performance, frame generation 2x, no cap, big pools, High priority' }
}
$sl = Join-Path $repo 'nvidia-sdk\Streamline\bin\x64'
$victory = Join-Path $repo 'target\release\victory.exe'
$hags = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\GraphicsDrivers' -Name HwSchMode -ErrorAction SilentlyContinue).HwSchMode
$isWin11 = [Environment]::OSVersion.Version.Build -ge 22000
$hagsOff = ($hags -eq 1) -or ($null -eq $hags -and -not $isWin11)
$hagsText = if ($hags -eq 2) {
    'on'
} elseif ($null -eq $hags -and $isWin11) {
    'default (on in Windows 11)'
} else {
    'off: frame generation needs it (Settings > Display > Graphics)'
}

if ($Check) {
    Write-Host "repo:    $repo"
    Write-Host "game:    $gameExe"
    Write-Host "mode:    $Mode ($what)"
    Write-Host "DLSS:    $(if (Test-Path (Join-Path $sl 'sl.interposer.dll')) { 'present' } else { 'not yet (downloaded at first start)' })"
    Write-Host "built:   $(if (Test-Path $victory) { 'yes' } else { 'no (built at first start)' })"
    Write-Host "HAGS:    $hagsText"
    exit 0
}
if (Get-Process victory -ErrorAction SilentlyContinue) { Write-Host 'The game is already running. Close it first.'; exit 1 }

# 2. Saves backup, verified file by file.
if (-not $NoBackup) {
    $src = Join-Path $env:LOCALAPPDATA 'victory\data\7952987A'
    if (Test-Path $src) {
        $root = if (Test-Path 'D:\') { 'D:\victory-save-backups' } else { Join-Path $env:USERPROFILE 'victory-save-backups' }
        $dst = Join-Path $root ('7952987A-{0:yyyyMMdd-HHmmss}-before-nvidia' -f (Get-Date))
        New-Item -ItemType Directory -Force $root | Out-Null
        Copy-Item $src $dst -Recurse -Force
        $files = @(Get-ChildItem $src -Recurse -File)
        $bad = 0
        foreach ($f in $files) {
            $t = Join-Path $dst $f.FullName.Substring($src.Length)
            try {
                if (-not (Test-Path $t) -or (Get-FileHash $f.FullName).Hash -ne (Get-FileHash $t).Hash) { $bad++ }
            } catch { $bad++ }
        }
        if ($bad) { Write-Host "Save backup check failed ($bad of $($files.Count) files). Not starting."; exit 1 }
        Write-Host "Saves backed up and checked: $dst ($($files.Count) files)"
    }
}

# 3. NVIDIA's DLSS and frame generation files, once.
if (-not (Test-Path (Join-Path $sl 'sl.interposer.dll'))) {
    Write-Host 'Getting NVIDIA DLSS files (about 70 MB, once) ...'
    try { & (Join-Path $PSScriptRoot 'get-nvidia-sdks.ps1') }
    catch { Write-Host "Could not get them ($($_.Exception.Message)): the game runs without DLSS and frame generation." }
}
if ($hagsOff) { Write-Host 'Hardware-accelerated GPU scheduling is off: frame generation will not work until it is on.' }

# 4. Start (play-foxhound.ps1 builds target\release\victory.exe the first time).
if (-not (Test-Path $victory)) { Write-Host 'First start: building the layer, a few minutes ...' }
Write-Host "Starting the title: $what"
Write-Host 'In the game: F1 settings menu, F3 stats panel. Keep this window open while playing.'
$env:VICTORY_GAME_DIR = $GameDir
$ErrorActionPreference = 'Continue'
# foxhound reads these but play-foxhound.ps1 does not set them: AnyXboxX's worker pools for 16/24+ logical CPUs
# (measured on an RTX 4070 SUPER: heavy scenes +22-29 %) and Vice-Tay's placed order. title_NO_TUNE=1: leave them out.
$cpus = [Environment]::ProcessorCount
$more = @{ VICTORY_PLACED_ORDER = '1' }
if ($cpus -ge 24) { $more.VICTORY_THIN_THREADS = '12'; $more.VICTORY_UNDER_THREADS = '8'; $more.VICTORY_LAND_THREADS = '8' }
elseif ($cpus -ge 16) { $more.VICTORY_THIN_THREADS = '12'; $more.VICTORY_UNDER_THREADS = '8'; $more.VICTORY_LAND_THREADS = '6' }
# Extreme: the landing pool sat full at 8 (x6.1-6.6 of 8 in AnyXboxX's heavy scenes), one more pipeline worker.
if (($Mode -in @('5', '6')) -and $cpus -ge 24) { $more.VICTORY_LAND_THREADS = '12'; $more.VICTORY_PIPELINE_WORKERS = '4' }
if ($env:title_NO_TUNE -eq '1') { $more = @{}; Write-Host 'title_NO_TUNE=1: foxhound defaults (no pool or placed-order tuning).' }
else { Write-Host ("Tuning for {0} logical CPUs: {1}" -f $cpus, (($more.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' ')) }
$opts.More = $more
# RTX 50 cards do multi frame generation: 4x there (the layer asks the driver and gives no more than it does).
# PLAY_FRAMEGEN_X=2..6 overrides. RTX 40 stays at 2x.
$gpuName = (Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | Where-Object { $_.Name -match 'NVIDIA' } | Select-Object -First 1).Name
if ($opts.FrameGen) {
    if ($env:PLAY_FRAMEGEN_X -match '^[2-6]$') { $opts.FrameGenX = [int]$env:PLAY_FRAMEGEN_X }
    elseif ($gpuName -match 'RTX\s*50\d\d') { $opts.FrameGenX = 4 }
    $fgx = if ($opts.FrameGenX) { $opts.FrameGenX } else { 2 }
    Write-Host ('GPU: {0}; frame generation {1}x' -f $gpuName, $fgx)
}
if ($Mode -in @('5', '6')) {
    # Raise the game to High priority once it is up (the submitting thread is the limit).
    $null = Start-Job -ArgumentList $repo -ScriptBlock {
        param($r)
        $end = (Get-Date).AddMinutes(15)
        while ((Get-Date) -lt $end) {
            $p = Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$r*" }
            if ($p) { $p | ForEach-Object { $_.PriorityClass = 'High' }; break }
            Start-Sleep -Seconds 2
        }
    }
    Write-Host 'Extreme: the game process goes to High priority once it starts.'
}
& (Join-Path $PSScriptRoot 'play-foxhound.ps1') -Exe $gameExe -Build release @opts
