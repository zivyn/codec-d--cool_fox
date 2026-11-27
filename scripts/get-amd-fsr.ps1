# Fetches the AMD files FSR 3 frame generation needs (about 30 MB) from AMD's own GitHub (the FidelityFX SDK,
# GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) into a fidelityfx folder: by default amd-sdk\FidelityFX\bin here
# (git-ignored), where scripts\play-foxhound.ps1 looks for them; `victory get-fsr` names one under
# %LOCALAPPDATA%\victory. They are AMD's, under AMD's licence (saved beside them: read it); this repository does
# not carry them, so each person gets them from AMD directly. Each file is checked against the SHA-256 of the
# copy this fork was tested with, and the game loads them only with AMD's valid signature.
param(
    [string]$Tag = 'v2.4.0',
    [string]$To = ''
)
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$bin = if ($To) { $To } else { Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..')).Path 'amd-sdk\FidelityFX\bin' }
$base = "https://raw.githubusercontent.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/$Tag/Kits/FidelityFX"
# What the game loads, and the SHA-256 of the copies tested (FidelityFX SDK v2.4.0).
$files = [ordered]@{
    'amd_fidelityfx_loader_dx12.dll'          = 'E2191D3C0828BE45EB8AFFD4E3EC0378F2293ADC345B772A7170EDCF3B021ECB'
    'amd_fidelityfx_framegeneration_dx12.dll' = 'E6843252F58B0341A0D80F6A20BC0D840643EE95BC48C22CAC08745344577BB6'
}

function Get-File([string]$Url, [string]$To) {
    New-Item -ItemType Directory -Force (Split-Path $To) | Out-Null
    $part = "$To.part"
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        & curl.exe --location --fail --silent --show-error --progress-bar --output $part $Url
        if ($LASTEXITCODE -ne 0) { Remove-Item $part -ErrorAction SilentlyContinue; throw "could not get $Url" }
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing -ErrorAction Stop
    }
    $part
}

Write-Host "AMD FSR 3 frame generation (FidelityFX SDK $Tag), from github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK (about 30 MB):"
New-Item -ItemType Directory -Force $bin | Out-Null
foreach ($name in $files.Keys) {
    $to = Join-Path $bin $name
    if ((Test-Path $to) -and (Get-FileHash $to -Algorithm SHA256).Hash -eq $files[$name]) {
        Write-Host "  $name already there"
        continue
    }
    $part = Get-File "$base/signedbin/$name" $to
    $hash = (Get-FileHash $part -Algorithm SHA256).Hash
    if ($hash -ne $files[$name]) {
        Remove-Item $part -Force
        throw "$name is not the file this build was tested with (SHA-256 $hash); not kept"
    }
    Move-Item $part $to -Force
    $len = (Get-Item $to).Length
    Write-Host ("  {0}  {1}" -f $name, $(if ($len -ge 1MB) { '{0:N1} MB' -f ($len / 1MB) } else { '{0:N0} KB' -f ($len / 1KB) }))
}
try {
    $lic = Get-File "$base/docs/license.md" (Join-Path $bin 'amd-fidelityfx-license.md')
    Move-Item $lic (Join-Path $bin 'amd-fidelityfx-license.md') -Force
    Write-Host "  licence: $(Join-Path $bin 'amd-fidelityfx-license.md')"
} catch { Write-Host "  (AMD's licence text could not be fetched: $($_.Exception.Message))" }
Write-Host ''
Write-Host 'Done. They are used from the next start.'
