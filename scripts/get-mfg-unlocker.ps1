# Fetches MFG Unlocker (github.com/dashdogy/MFG-Unlocker, someone else's MIT-licensed mod, about 5 MB) into
# %LOCALAPPDATA%\victory\mfg-unlocker, or the folder -To names. It lets DLSS Frame Generation run on GeForce
# RTX 20 and 30 cards, which NVIDIA does not support. victory.exe loads it only with the F1 menu's MFG UNLOCKER
# on (VICTORY_MFG_UNLOCKER=1); nothing is renamed or put beside the game. It is not part of this repository and
# was not tested by this fork. Its release v1.7.2 is pinned: the zip and the DLL in it are checked against the
# SHA-256 the mod's author published with that release (SHA256SUMS.txt), and nothing else is kept.
param(
    [string]$To = ''
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$dir = if ($To) { $To } else { Join-Path $env:LOCALAPPDATA 'victory\mfg-unlocker' }
$version = '1.7.2'
# From the release's SHA256SUMS.txt (also GitHub's own digest of the zip).
$package = 'E002D624E3E7BA5CEDAB7211043C096FE8F555C30FF43500235A0686C7893F22'
$module = 'A9B1C01727FA72763BEF8F604E44EC165CE1C98272DE00719572B766A11CF28F'
$url = "https://github.com/dashdogy/MFG-Unlocker/releases/download/v$version/MFGUnlocker-v$version.zip"

$target = Join-Path $dir 'MFGUnlocker.dll'
if ((Test-Path $target) -and (Get-FileHash $target -Algorithm SHA256).Hash -eq $module) {
    Write-Host "MFG Unlocker $version is already in $dir"
    exit 0
}
Write-Host "MFG Unlocker $version (someone else's mod), from github.com/dashdogy/MFG-Unlocker (about 5 MB):"
New-Item -ItemType Directory -Force $dir | Out-Null
$part = Join-Path $dir "MFGUnlocker-v$version.zip.part"
try {
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        & curl.exe --location --fail --silent --show-error --progress-bar --output $part $url
        if ($LASTEXITCODE -ne 0) { throw "could not get $url" }
    } else {
        Invoke-WebRequest -Uri $url -OutFile $part -UseBasicParsing
    }
    $hash = (Get-FileHash $part -Algorithm SHA256).Hash
    if ($hash -ne $package) { throw "the zip is not the v$version release (SHA-256 $hash); not kept" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($part)
    try {
        $entry = $zip.Entries | Where-Object Name -eq 'MFGUnlocker.dll' | Select-Object -First 1
        if (-not $entry) { throw 'the zip has no MFGUnlocker.dll' }
        $dll = "$target.part"
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $dll, $true)
        # Its licence and notices (Intel's XeSS data in it is under Intel's licence), kept beside it.
        foreach ($name in 'LICENSE', 'LICENSE.txt', 'NOTICE.md', 'NOTICE.txt', 'README.md', 'README.txt') {
            $doc = $zip.Entries | Where-Object Name -eq $name | Select-Object -First 1
            if ($doc) { [IO.Compression.ZipFileExtensions]::ExtractToFile($doc, (Join-Path $dir $name), $true) }
        }
    } finally { $zip.Dispose() }
    $hash = (Get-FileHash $dll -Algorithm SHA256).Hash
    if ($hash -ne $module) { Remove-Item $dll -Force; throw "MFGUnlocker.dll is not the v$version file (SHA-256 $hash); not kept" }
    Move-Item $dll $target -Force
} finally {
    Remove-Item $part -Force -ErrorAction SilentlyContinue
}
Write-Host ('  MFGUnlocker.dll  {0:N1} MB, v{1}, checked against the published SHA-256' -f ((Get-Item $target).Length / 1MB), $version)
Write-Host "  in $dir (its licence and notices beside it)"
