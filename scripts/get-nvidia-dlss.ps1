# Fetches the NVIDIA files DLSS needs (about 70 MB) from NVIDIA's own GitHub (github.com/NVIDIA-RTX/Streamline) into
# <home>\nvidia-sdk\Streamline\bin\x64, where victory.exe looks for them. They are NVIDIA's, under NVIDIA's licences
# (saved beside them: read them); this repository and its releases do not carry them and may not, so each person gets
# them from NVIDIA directly. Nothing is downloaded unless you say yes (the prompt) or pass -Yes.
#
#   get-nvidia-dlss.ps1            asks first, then fetches
#   get-nvidia-dlss.ps1 -Yes       fetches without asking (your flag is the consent)
#   get-nvidia-dlss.ps1 -Ask       what Play-VIctory.cmd runs at every start: says nothing and does nothing unless the
#                                  files are missing, a GeForce RTX is there, a person is at the console and they
#                                  have not said no before; then asks once
#   -To <folder>                   the home folder to use (as VICTORY_HOME)
#   -ExeDir <folder>               the folder of victory.exe (Play-VIctory.cmd passes it)
#   -Where                         only prints the folder the files go to (and are looked for in), writes nothing
#
# The home folder is the one victory.exe looks in (crates\victory-gpu\src\streamline\choose.rs): -To, else VICTORY_HOME,
# else victory-home beside the folder of VICTORY_CACHE_DIR (the cache's drive; also read from cache_dir in the config
# file), else victory-home beside victory.exe. Never %LOCALAPPDATA%: nothing is written to the system drive here (the
# temporary files go into the home folder too). VICTORY_STREAMLINE_DIR set means the files are yours: nothing is done.
#
# The zip-in-place fetch is adapted from the VIctory-NVIDIA fork's get-nvidia-sdks.ps1 (GPL-3.0-or-later).
param(
    [switch]$Yes,
    [switch]$Ask,
    [switch]$Where,
    [string]$To = '',
    [string]$ExeDir = '',
    [string]$StreamlineTag = 'v2.14.16'
)
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest's progress bar makes it many times slower
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# What is loaded when the game runs: Streamline's interposer and plugins, DLSS and the frame generation model.
$dlls = 'sl.interposer.dll', 'sl.common.dll', 'sl.dlss.dll', 'sl.dlss_g.dll', 'sl.reflex.dll', 'sl.pcl.dll', 'nvngx_dlss.dll', 'nvngx_dlssg.dll'

if ($env:VICTORY_STREAMLINE_DIR) {
    if ($Where) { Write-Output $env:VICTORY_STREAMLINE_DIR; return }
    if (-not $Ask) { Write-Host "VICTORY_STREAMLINE_DIR is set ($env:VICTORY_STREAMLINE_DIR): that folder is yours, nothing is fetched. Unset it to use the home folder." }
    return
}
if ($Ask -and $env:VICTORY_DLSS -match '^\s*(0|off|false|no)\s*$') { return }

# The folder named by cache_dir in the config file victory.exe reads (VICTORY_CONFIG, else victory.toml here), if any.
function Get-ConfigCacheDir {
    $file = if ($env:VICTORY_CONFIG) { $env:VICTORY_CONFIG } else { 'victory.toml' }
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { return $null }
    foreach ($line in (Get-Content -LiteralPath $file -TotalCount 200)) {
        if ($line -match '^\s*cache_dir\s*=\s*"((?:[^"\\]|\\.)*)"') { return ($Matches[1] -replace '\\\\', '\') }
        if ($line -match "^\s*cache_dir\s*=\s*'([^']*)'") { return $Matches[1] }
    }
    return $null
}

if (-not $ExeDir) { $ExeDir = $PSScriptRoot; if ((Split-Path $ExeDir -Leaf) -eq 'scripts') { $ExeDir = Split-Path $ExeDir } }
$cache = if ($env:VICTORY_CACHE_DIR) { $env:VICTORY_CACHE_DIR } else { Get-ConfigCacheDir }
$home_ = if ($To) { $To }
    elseif ($env:VICTORY_HOME) { $env:VICTORY_HOME }
    elseif ($cache) {
        $parent = Split-Path ([IO.Path]::GetFullPath($cache).TrimEnd('\')) -Parent
        if ($parent) { [IO.Path]::Combine($parent, 'victory-home') } else { [IO.Path]::Combine([IO.Path]::GetFullPath($cache), 'victory-home') }
    }
    else { [IO.Path]::Combine([IO.Path]::GetFullPath($ExeDir), 'victory-home') }
$home_ = [IO.Path]::GetFullPath($home_)
$sdk = [IO.Path]::Combine($home_, 'nvidia-sdk')
$sl = [IO.Path]::Combine($sdk, 'Streamline')
$bin = [IO.Path]::Combine($sl, 'bin', 'x64')

if ($Where) { Write-Output $bin; return }
$missing = @($dlls | Where-Object { -not (Test-Path -LiteralPath (Join-Path $bin $_)) })
if ($Ask) {
    # The launcher's call: only when something is to be offered.
    if (-not $missing) { return }
    if (Test-Path -LiteralPath (Join-Path $home_ 'dlss-declined.txt')) { return }
    if ([Console]::IsInputRedirected -or -not [Environment]::UserInteractive) { return }
    $rtx = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | Where-Object { $_.Name -match 'NVIDIA' -and $_.Name -match 'RTX' })
    if (-not $rtx) { return }
}
if (-not $missing) {
    Write-Host "NVIDIA's DLSS files are all there already: $bin"
    return
}

Write-Host ''
Write-Host "DLSS (NVIDIA's upscaler) needs NVIDIA's files: Streamline $StreamlineTag with DLSS and frame generation."
Write-Host "  From: github.com/NVIDIA-RTX/Streamline (NVIDIA's own release), about 70 MB, once."
Write-Host "  Into: $bin"
Write-Host "  They are NVIDIA's, under NVIDIA's licences (saved beside them: read them). This program does not contain them."
Write-Host '  Without them the game runs with its own upscaling.'
if (-not $Yes) {
    $answer = Read-Host 'Download them now? [y/N]'
    if ($answer.Trim().ToLowerInvariant() -notin 'y', 'yes') {
        New-Item -ItemType Directory -Force $home_ | Out-Null
        Set-Content -LiteralPath (Join-Path $home_ 'dlss-declined.txt') -Value 'said no once; Play-VIctory.cmd get-dlss fetches them' -Encoding ASCII
        Write-Host 'Not downloaded. "Play-VIctory.cmd get-dlss" does it later.'
        return
    }
}
Remove-Item -LiteralPath (Join-Path $home_ 'dlss-declined.txt') -ErrorAction SilentlyContinue

# Temporary files go to the home folder too, not to the user profile on the system drive.
$tmp = Join-Path $home_ 'tmp'
New-Item -ItemType Directory -Force $tmp | Out-Null
$env:TEMP = $tmp
$env:TMP = $tmp

# One file from a URL, with curl's progress line where Windows has curl (it has since Windows 10 1803).
function Get-File([string]$Url, [string]$To) {
    New-Item -ItemType Directory -Force (Split-Path $To) | Out-Null
    $part = "$To.part"
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        & curl.exe --location --fail --silent --show-error --progress-bar --output $part $Url
        if ($LASTEXITCODE -ne 0) { Remove-Item $part -ErrorAction SilentlyContinue; throw "could not get $Url" }
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing -ErrorAction Stop
    }
    Move-Item $part $To -Force
}

# Bytes first..last of a URL.
function Get-Range([string]$Url, [long]$First, [long]$Last) {
    $q = [Net.HttpWebRequest]::Create($Url)
    $q.UserAgent = 'get-nvidia-dlss'
    $q.AddRange($First, $Last)
    $r = $q.GetResponse()
    try {
        if ([int]$r.StatusCode -ne 206) { throw 'the server does not give parts of the file' }
        $m = New-Object IO.MemoryStream
        $r.GetResponseStream().CopyTo($m)
        , $m.ToArray()
    } finally { $r.Close() }
}

# Takes the named files out of a zip on a server without downloading the whole zip: its directory is at
# its end, and each file in it can be asked for by its place. $Want: a regex on the paths in the zip.
# Each goes into $Into under the part of its path after the SDK's top folder.
function Get-FromZip([string]$Url, [string]$Want, [string]$Into) {
    # Where the file really is (the download address redirects), and how long it is.
    $h = [Net.HttpWebRequest]::Create($Url); $h.Method = 'HEAD'; $h.UserAgent = 'get-nvidia-dlss'
    $r = $h.GetResponse(); $real = $r.ResponseUri.AbsoluteUri; $length = $r.ContentLength; $r.Close()
    $tailLen = [Math]::Min($length, 2MB)
    $tail = Get-Range $real ($length - $tailLen) ($length - 1)
    $end = -1
    for ($i = $tail.Length - 22; $i -ge 0; $i--) {
        if ($tail[$i] -eq 0x50 -and $tail[$i + 1] -eq 0x4b -and $tail[$i + 2] -eq 5 -and $tail[$i + 3] -eq 6) { $end = $i; break }
    }
    if ($end -lt 0) { throw 'no zip directory found' }
    $dirSize = [BitConverter]::ToUInt32($tail, $end + 12)
    $dirAt = [BitConverter]::ToUInt32($tail, $end + 16)
    if ($dirAt -eq [uint32]::MaxValue) { throw 'a zip64 archive' }
    $dir = if ($dirAt -ge $length - $tailLen) {
        $from = [int]($dirAt - ($length - $tailLen)); , $tail[$from..($from + $dirSize - 1)]
    } else { Get-Range $real $dirAt ($dirAt + $dirSize - 1) }
    $got = 0
    $at = 0
    while ($at + 46 -le $dir.Length -and [BitConverter]::ToUInt32($dir, $at) -eq 0x02014b50) {
        $method = [BitConverter]::ToUInt16($dir, $at + 10)
        $packed = [BitConverter]::ToUInt32($dir, $at + 20)
        $size = [BitConverter]::ToUInt32($dir, $at + 24)
        $nameLen = [BitConverter]::ToUInt16($dir, $at + 28)
        $extraLen = [BitConverter]::ToUInt16($dir, $at + 30)
        $noteLen = [BitConverter]::ToUInt16($dir, $at + 32)
        $headerAt = [BitConverter]::ToUInt32($dir, $at + 42)
        $name = [Text.Encoding]::UTF8.GetString($dir, $at + 46, $nameLen)
        $at += 46 + $nameLen + $extraLen + $noteLen
        if ($name -notmatch $Want) { continue }
        # The file's own header (its extra field may be longer than the directory's), then its bytes.
        $piece = Get-Range $real $headerAt ($headerAt + 30 + $nameLen + 2048 + $packed - 1)
        if ([BitConverter]::ToUInt32($piece, 0) -ne 0x04034b50) { throw "no file header for $name" }
        $dataAt = 30 + [BitConverter]::ToUInt16($piece, 26) + [BitConverter]::ToUInt16($piece, 28)
        if ($dataAt + $packed -gt $piece.Length) { throw "the header of $name is longer than expected" }
        $out = [IO.MemoryStream]::new()
        if ($method -eq 0) { $out.Write($piece, $dataAt, [int]$packed) }
        elseif ($method -eq 8) {
            $in = [IO.MemoryStream]::new($piece, $dataAt, [int]$packed)
            $z = [IO.Compression.DeflateStream]::new($in, [IO.Compression.CompressionMode]::Decompress)
            $z.CopyTo($out); $z.Close()
        } else { throw "$name is packed in a way this script does not know ($method)" }
        if ($out.Length -ne $size) { throw "$name came out $($out.Length) bytes long, not $size" }
        $rel = ($name -replace '^.*?(?=(bin/x64/|license\.txt$))', '') -replace '/', '\'
        $to = Join-Path $Into $rel
        New-Item -ItemType Directory -Force (Split-Path $to) | Out-Null
        [IO.File]::WriteAllBytes($to, $out.ToArray())
        Write-Host ("  {0}  {1:N1} MB" -f $rel, ($size / 1MB))
        $got++
    }
    $got
}

try {
    $url = "https://github.com/NVIDIA-RTX/Streamline/releases/download/$StreamlineTag/streamline-sdk-$StreamlineTag.zip"
    $names = ($dlls | ForEach-Object { [regex]::Escape($_) }) -join '|'
    $want = "(^|/)bin/x64/($names|[^/]*license[^/]*\.txt)$|^([^/]+/)?license\.txt$"
    try { [void](Get-FromZip $url $want $sl) }
    catch { Write-Host "  Taking the files out of the archive in place did not work ($($_.Exception.Message))." }
    if (@($dlls | Where-Object { -not (Test-Path -LiteralPath (Join-Path $bin $_)) })) {
        # The whole archive then (263 MB), unpacked in the home folder.
        Write-Host '  Getting the whole archive instead (263 MB) ...'
        $zip = Join-Path $sdk "streamline-sdk-$StreamlineTag.zip"
        Get-File $url $zip
        $into = Join-Path $sdk 'streamline-unpacked'
        Expand-Archive -Path $zip -DestinationPath $into -Force -ErrorAction Stop
        $found = Get-ChildItem $into -Recurse -Filter 'sl.interposer.dll' |
            Where-Object { $_.FullName -match '\\bin\\x64\\sl\.interposer\.dll$' -and $_.FullName -notmatch '\\development\\' } |
            Select-Object -First 1
        if (-not $found) { throw 'The Streamline archive has no bin\x64\sl.interposer.dll.' }
        $top = Split-Path (Split-Path (Split-Path $found.FullName))
        New-Item -ItemType Directory -Force $bin | Out-Null
        Get-ChildItem (Join-Path $top 'bin\x64') -File |
            Where-Object { $dlls -contains $_.Name -or $_.Name -match 'license.*\.txt$' } |
            Copy-Item -Destination $bin -Force
        Copy-Item (Join-Path $top 'license.txt') $sl -Force -ErrorAction SilentlyContinue
        Remove-Item $into -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item $zip -Force
    }
    $still = @($dlls | Where-Object { -not (Test-Path -LiteralPath (Join-Path $bin $_)) })
    if ($still) { throw "Not there after all: $($still -join ', ')" }
    # (victory.exe checks the signature of the interposer itself before it loads it; this shows it early.)
    $unsigned = @($dlls | Where-Object { (Get-AuthenticodeSignature -LiteralPath (Join-Path $bin $_)).Status -ne 'Valid' })
    if ($unsigned) { Write-Host "  Warning: these are not validly signed, DLSS will not load them: $($unsigned -join ', ')" }
    Write-Host "  licences: $(Join-Path $sl 'license.txt') (Streamline) and the *.license.txt files in $bin (DLSS, Reflex)"
    Write-Host ''
    Write-Host 'Done. DLSS is used from the next start.'
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
